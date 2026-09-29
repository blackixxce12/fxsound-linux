/*
 * The golden vectors of `at_interface_and_sound_every_stored_value_gets_the_windows_builds_parameters`
 * (crates/fxsound-dsp/src/effects/ambience.rs): Ambience's parameters in the Windows build for
 * every stored value 0 to 127, as (runs, decay, lat6 coefficient, wet, dry) —
 * dfxp_CommunicateAmbience and dfxp_CommAmbienceBypass (dfxpComm.cpp:571-679, 1662-1691) with the
 * two quantisers they read (qntIToRInit's QNT_RESPONSE_EXP and QNT_RESPONSE_LINEAR, Qntitor.cpp;
 * dfxpQnt.cpp:144-184), transcribed statement for statement with realtype = float and compiled
 * as C with glibc's libm on x86-64 (0.4.0 audit #39, whose reference is the Windows C code).
 * The constants are the Windows headers' (c_play.h, c_lex.h, dfxpDefs.h, dfxpComm.cpp).
 */
#include <stdio.h>
#include <math.h>
typedef float realtype;
#define MIDI_MIN_VALUE 0
#define MIDI_MAX_VALUE 127
#define PLY_DECAY_MIN_VALUE 0.095
#define PLY_DECAY_MAX_VALUE 0.95
#define DSP_LEX_ROOM_SIZE_MIN_VALUE 0.5
#define DSP_LEX_ROOM_SIZE_MAX_VALUE 1.5
#define DSP_PLAY_LEX_ROOM_SIZE_MIDI 64
#define DFXP_MUSIC_MODE2_AMBIENCE_FACTOR 0.34
#define DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE 12

static realtype decay_array[128];
static realtype room_array[128];

static void qnt_exp(realtype *real_array, int i_input_min, int i_input_max,
                    realtype r_output_min, realtype r_output_max) {
  int array_size = i_input_max - i_input_min + 1, index;
  realtype factor;
  factor = (realtype)pow((double)(r_output_max/r_output_min), (double)(1.0/i_input_max));
  real_array[0] = r_output_min;
  real_array[(array_size - 1)] = r_output_max;
  for (index=1; index < (array_size - 1); index++)
    real_array[index] = real_array[index - 1] * factor;
}

static void qnt_linear(realtype *real_array, int i_input_min, int i_input_max,
                       realtype r_output_min, realtype r_output_max) {
  int array_size = i_input_max - i_input_min + 1, index;
  realtype scale = (r_output_max - r_output_min)/(i_input_max - i_input_min);
  for (index=0; index < (array_size); index++)
    real_array[index] = r_output_min + scale * index;
  real_array[(array_size - 1)] = r_output_max;
}

int main(void) {
  qnt_exp(decay_array, MIDI_MIN_VALUE, MIDI_MAX_VALUE, (realtype)PLY_DECAY_MIN_VALUE, (realtype)PLY_DECAY_MAX_VALUE);
  qnt_linear(room_array, MIDI_MIN_VALUE, MIDI_MAX_VALUE, (realtype)DSP_LEX_ROOM_SIZE_MIN_VALUE, (realtype)DSP_LEX_ROOM_SIZE_MAX_VALUE);
  for (int stored = 0; stored <= 127; stored++) {
    int pc_liveliness = stored;
    realtype dsp_decay, dsp_lat6_coeff, roomsize, wet_gain, dry_gain;
    int ambience_bypass;
    pc_liveliness = (int)((realtype)pc_liveliness * DFXP_MUSIC_MODE2_AMBIENCE_FACTOR);
    roomsize = room_array[DSP_PLAY_LEX_ROOM_SIZE_MIDI];
    dsp_decay = decay_array[pc_liveliness];
    dsp_decay = (realtype)pow(dsp_decay, roomsize);
    dsp_lat6_coeff = dsp_decay + (realtype)0.15;
    if( dsp_lat6_coeff < (realtype)0.25 ) dsp_lat6_coeff = (realtype)0.25;
    if( dsp_lat6_coeff > (realtype)0.5 ) dsp_lat6_coeff = (realtype)0.5;
    if( pc_liveliness > 40 ) {
      wet_gain = (realtype)(0.21 * 1.3);
      dry_gain = (realtype)(0.69 * 1.3);
    } else {
      wet_gain = (realtype)((pc_liveliness - 12) * (1.0/(40 - 12)) * (0.21 * 1.3));
      dry_gain = (realtype)0.897 + (realtype)((40 - pc_liveliness) * (1.0/(40 - 12))) * (realtype)(1.0 - 0.897);
    }
    ambience_bypass = (stored <= DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE);
    printf("    (%s, %.9e, %.9e, %.9e, %.9e),\n", ambience_bypass ? "false" : "true",
           dsp_decay, dsp_lat6_coeff, wet_gain, dry_gain);
  }
  return 0;
}
