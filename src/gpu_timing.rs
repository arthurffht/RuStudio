//! RUSTUDIO_GPU_TIMING=1: GPU time per pass group, from D3D11 timestamp queries.
//!
//! `begin(device, ctx)` starts a frame's disjoint block and writes a timestamp;
//! each `mark(ctx, label)` writes another and books the GPU time since the
//! previous one to `label`. `scope(device, ctx, label, f)` times a standalone
//! pass without disturbing an active `begin`/`mark` run. `end_frame` closes
//! the disjoint block and polls completed frames in the ring non-blockingly
//! with `D3D11_ASYNC_GETDATA_DONOTFLUSH`. Results lag the frame they measure by
//! a frame or two.

use std::sync::{Mutex, OnceLock};

use windows::core::Interface;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Direct3D11::*;

const MAX_QUERIES: u32 = 256;
const RING: usize = 4;
const FREE: u8 = 0;
const RECORDING: u8 = 1;
const PENDING: u8 = 2;

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RUSTUDIO_GPU_TIMING").is_some())
}

struct Slot {
    disjoint: ID3D11Query,
    queries: Vec<ID3D11Query>,
    spans: Vec<(u32, u32, &'static str)>,
    used: u32,
    state: u8,
}

// SAFETY: D3D11 COM interfaces (`ID3D11Query`) are thread-safe to hold behind `Mutex`
// (CreateQuery / End / GetData are only invoked from the render thread holding the context).
unsafe impl Send for Slot {}

struct State {
    slots: Vec<Slot>,
    cur: Option<usize>,
    last: Option<u32>,
    results: Vec<(&'static str, f64)>,
    totals: Vec<f64>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static UNSUPPORTED: OnceLock<()> = OnceLock::new();

fn init(device: &ID3D11Device) -> Option<State> {
    let dj_desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_TIMESTAMP_DISJOINT,
        MiscFlags: 0,
    };
    let ts_desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_TIMESTAMP,
        MiscFlags: 0,
    };
    let mut slots = Vec::with_capacity(RING);
    for _ in 0..RING {
        let mut dj = None;
        if unsafe { device.CreateQuery(&dj_desc, Some(&mut dj)) }.is_err() {
            UNSUPPORTED.get_or_init(|| {
                eprintln!("RUSTUDIO_GPU_TIMING: D3D11 timestamp disjoint queries unsupported")
            });
            return None;
        }
        let disjoint = dj?;
        let mut queries = Vec::with_capacity(MAX_QUERIES as usize);
        for _ in 0..MAX_QUERIES {
            let mut q = None;
            if unsafe { device.CreateQuery(&ts_desc, Some(&mut q)) }.is_err() {
                UNSUPPORTED.get_or_init(|| {
                    eprintln!("RUSTUDIO_GPU_TIMING: D3D11 timestamp queries unsupported")
                });
                return None;
            }
            queries.push(q?);
        }
        slots.push(Slot {
            disjoint,
            queries,
            spans: Vec::new(),
            used: 0,
            state: FREE,
        });
    }
    Some(State {
        slots,
        cur: Some(0),
        last: None,
        results: Vec::new(),
        totals: Vec::new(),
    })
}

fn ensure_recording(slot: &mut Slot, ctx: &ID3D11DeviceContext) {
    if slot.state == FREE {
        unsafe { ctx.Begin(&slot.disjoint) };
        slot.state = RECORDING;
    }
}

fn write(st: &mut State, ctx: &ID3D11DeviceContext) -> Option<u32> {
    let slot = &mut st.slots[st.cur?];
    ensure_recording(slot, ctx);
    if slot.used >= MAX_QUERIES {
        return None;
    }
    let i = slot.used;
    unsafe { ctx.End(&slot.queries[i as usize]) };
    slot.used += 1;
    Some(i)
}

fn get_data<T: Copy>(ctx: &ID3D11DeviceContext, query: &ID3D11Query, flags: u32) -> Option<T> {
    let mut val = std::mem::MaybeUninit::<T>::zeroed();
    let async_obj: ID3D11Asynchronous = query.cast().ok()?;
    // Call raw vtable directly so `S_FALSE` (1) is distinguished from `S_OK` (0).
    let hr = unsafe {
        (Interface::vtable(ctx).GetData)(
            Interface::as_raw(ctx),
            Interface::as_raw(&async_obj),
            val.as_mut_ptr().cast(),
            std::mem::size_of::<T>() as u32,
            flags,
        )
    };
    if hr == S_OK {
        Some(unsafe { val.assume_init() })
    } else {
        None
    }
}

/// Starts a timed run of work on `ctx`.
pub fn begin(device: &ID3D11Device, ctx: &ID3D11DeviceContext) {
    if !on() || UNSUPPORTED.get().is_some() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = init(device);
    }
    let Some(st) = g.as_mut() else { return };
    st.last = write(st, ctx);
}

/// Books the GPU time since the last `begin` / `mark` on `ctx` to `label`.
pub fn mark(ctx: &ID3D11DeviceContext, label: &'static str) {
    if !on() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    let Some(prev) = st.last else { return };
    let Some(i) = write(st, ctx) else { return };
    let cur = st.cur.unwrap_or(0);
    st.slots[cur].spans.push((prev, i, label));
    st.last = Some(i);
}

/// Once per frame, after the frame's work is issued: close this frame's
/// disjoint block and collect earlier frames that finished without flushing.
pub fn end_frame(_device: &ID3D11Device, ctx: &ID3D11DeviceContext) {
    if !on() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    if let Some(cur) = st.cur {
        let slot = &mut st.slots[cur];
        if slot.state == RECORDING {
            unsafe { ctx.End(&slot.disjoint) };
            if slot.used > 0 && !slot.spans.is_empty() {
                slot.state = PENDING;
            } else {
                slot.used = 0;
                slot.spans.clear();
                slot.state = FREE;
            }
        } else {
            slot.used = 0;
            slot.spans.clear();
        }
    }
    st.last = None;
    let flags = D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32;
    let mut ts = Vec::new();
    for slot in st.slots.iter_mut() {
        if slot.state != PENDING {
            continue;
        }
        let Some(dj) = get_data::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(ctx, &slot.disjoint, flags)
        else {
            continue;
        };
        if dj.Disjoint.as_bool() || dj.Frequency == 0 {
            slot.used = 0;
            slot.spans.clear();
            slot.state = FREE;
            continue;
        }
        ts.clear();
        let mut all_ready = true;
        for q in &slot.queries[..slot.used as usize] {
            match get_data::<u64>(ctx, q, flags) {
                Some(v) => ts.push(v),
                None => {
                    all_ready = false;
                    break;
                }
            }
        }
        if !all_ready {
            continue;
        }
        let freq = dj.Frequency as f64;
        let mut total = 0.0;
        for (a, b, label) in &slot.spans {
            let ms = ts[*b as usize].wrapping_sub(ts[*a as usize]) as f64 * 1000.0 / freq;
            if !(0.0..1000.0).contains(&ms) {
                continue;
            }
            total += ms;
            match st.results.iter_mut().find(|e| e.0 == *label) {
                Some(e) => e.1 += ms,
                None => st.results.push((label, ms)),
            }
        }
        st.totals.push(total);
        slot.used = 0;
        slot.spans.clear();
        slot.state = FREE;
    }
    st.cur = st.slots.iter().position(|s| s.state == FREE);
}

/// The GPU milliseconds per label since the last call, and each finished
/// frame's total.
pub fn take() -> (Vec<(&'static str, f64)>, Vec<f64>) {
    if !on() {
        return (Vec::new(), Vec::new());
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    match g.as_mut() {
        Some(st) => (std::mem::take(&mut st.results), std::mem::take(&mut st.totals)),
        None => (Vec::new(), Vec::new()),
    }
}

/// Times `f` as `label` on `ctx` without disturbing an active `begin`/`mark` run.
pub fn scope<R>(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    label: &'static str,
    f: impl FnOnce() -> R,
) -> R {
    if !on() || UNSUPPORTED.get().is_some() {
        return f();
    }
    let (slot_idx, start) = {
        let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = init(device);
        }
        match g.as_mut() {
            Some(st) => (st.cur, write(st, ctx)),
            None => (None, None),
        }
    };
    let out = f();
    if let (Some(idx), Some(a)) = (slot_idx, start) {
        let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = g.as_mut() {
            if st.cur == Some(idx) && st.slots[idx].state == RECORDING {
                if let Some(b) = write(st, ctx) {
                    st.slots[idx].spans.push((a, b, label));
                }
            }
        }
    }
    out
}
