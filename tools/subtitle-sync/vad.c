#include <stdint.h>
#include <stdio.h>
#include "webrtc/common_audio/vad/include/webrtc_vad.h"
int main(void) {
    VadInst *vad = WebRtcVad_Create();
    if (!vad || WebRtcVad_Init(vad) || WebRtcVad_set_mode(vad, 2)) return 2;
    int16_t pcm[320];
    size_t count;
    unsigned frames = 0;
    while ((count = fread(pcm, sizeof(int16_t), 320, stdin)) != 0) {
        if (count != 320 || ++frames > 6000) return 3;
        int value = WebRtcVad_Process(vad, 16000, pcm, 320);
        if (value < 0 || fputc(value, stdout) == EOF) return 4;
    }
    WebRtcVad_Free(vad);
    return ferror(stdin) || fflush(stdout) ? 5 : 0;
}
