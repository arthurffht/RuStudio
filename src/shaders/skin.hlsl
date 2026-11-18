#include "common.hlsli"

RWByteAddressBuffer dst : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 gid : SV_DispatchThreadID) {
    if (gid.x == 0xFFFFFFFFu) {
        dst.Store(0, 0);
    }
}
