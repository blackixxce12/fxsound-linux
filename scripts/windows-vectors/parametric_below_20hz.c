/*
 * The golden vectors of `below_20hz_the_windows_dsp_designs_the_windows_builds_sections`
 * (crates/fxsound-dsp/src/biquad.rs): the Windows build's filtCalcParametric below 20 Hz, where
 * its Q cap goes negative (0.4.0 audit #12), made as docs/spec/09-dsp-eq.md §13's vectors are —
 * FiltCalcBiqd.cpp itself, compiled as C with realtype = float and glibc's libm on x86-64.
 * build.sh compiles it against a checkout of FxSound LLC's repository.
 */
#include <stdio.h>
#include "FiltCalcBiqd.cpp"

int main(void)
{
    const float fs = 48000.0f, q = 4.3336544f; /* 48 kHz, the 31-band ladder's Q */
    const float freqs[] = {10.0f, 15.0f, 17.9f, 19.99f};
    const float boosts[] = {6.0f, -6.0f, 12.0f, 3.0f};
    for (int i = 0; i < 4; i++) {
        for (int j = 0; j < 4; j++) {
            struct filt2ndOrderBoostCutShelfFilterType f = {0};
            f.r_samp_freq = fs;
            f.r_center_freq = freqs[i];
            f.boost = boosts[j];
            f.Q = q;
            filtCalcParametric(&f);
            printf("(%.9g, %.9g, [%.9e, %.9e, %.9e, %.9e]), // Q %.9g\n", freqs[i], boosts[j],
                   f.b0, f.b1, f.b2, f.a2, f.Q);
        }
    }
    return 0;
}
