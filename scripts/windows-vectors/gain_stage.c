/*
 * The golden vectors of `at_interface_and_sound_the_balance_plays_on_stereo_only_as_the_windows_c_code_has_it`
 * (crates/fxsound-dsp/src/engine.rs): a frame of the Windows build's gain stage on 2, 6 and 8
 * channels at -6 dB of master gain and +10 dB of balance (0.4.0 audit #44) — GraphicEqSetMasterGain
 * and GraphicEqSetBalance (GraphicEqSet.cpp:38-60, :92-104), which hand sosSetMasterGain and
 * sosSetBalance their powf, and the last statement of sosProcessBuffer's stereo case
 * (SosProcess.cpp:630-631) and of sosProcessSurroundBuffer (:904), with every section off (out =
 * in, SOS_DO_DC_BLOCKING undefined as in the Windows build), transcribed statement for statement
 * with realtype = float and compiled as C with glibc's libm on x86-64. Printed as the f32 bits
 * of (input, output) per channel.
 */
#include <stdio.h>
#include <string.h>
#include <math.h>
typedef float realtype;

static unsigned bits(realtype x) {
  unsigned u;
  memcpy(&u, &x, sizeof u);
  return u;
}

int main(void) {
  float gain_db = -6.0f, balance_db = 10.0f;
  /* GraphicEqSetMasterGain */
  float master_gain = powf(10.0f, gain_db / 20.0f);
  /* GraphicEqSetBalance */
  float balance_left = 1.0f;
  float balance_right = 1.0f;
  if (balance_db > 0.0f) {
    balance_left = powf(10.0f, -balance_db / 20.0f);
  } else if (balance_db < 0.0f) {
    balance_right = powf(10.0f, balance_db / 20.0f);
  }
  const int layouts[] = {2, 6, 8};
  for (int l = 0; l < 3; l++) {
    int i_num_channels = layouts[l];
    realtype rp_in_buf[8], rp_out_buf[8];
    for (int c = 0; c < i_num_channels; c++)
      rp_in_buf[c] = 0.1f + 0.07f * (realtype)c;
    if (i_num_channels == 2) {
      /* sosProcessBuffer, stereo case, every section off */
      realtype out1 = rp_in_buf[0], out2 = rp_in_buf[1];
      rp_out_buf[0] = out1 * master_gain * balance_left;
      rp_out_buf[1] = out2 * master_gain * balance_right;
    } else {
      /* sosProcessSurroundBuffer, every section off */
      for (int k = 0; k < i_num_channels; k++) {
        realtype out = rp_in_buf[k];
        rp_out_buf[k] = out * master_gain;
      }
    }
    printf("    (%d, &[", i_num_channels);
    for (int c = 0; c < i_num_channels; c++)
      printf("%s(0x%08x, 0x%08x)", c ? ", " : "", bits(rp_in_buf[c]), bits(rp_out_buf[c]));
    printf("]),\n");
  }
  return 0;
}
