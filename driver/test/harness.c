// Development harness for LS50Stream.driver: loads the plug-in outside coreaudiod, checks its
// properties and rate switching, waits for ls50streamd to publish the device, then plays audio
// through it while emulating the HAL's IO cycle. Adapted from Limbo's harness.
//   harness <path/to/LS50Stream.driver/Contents/MacOS/LS50Stream> <seconds> [audio.f32 [rate]]
// audio.f32: raw interleaved stereo float32 at `rate` (default 192000), e.g. from
//   ffmpeg -i song.mp3 -ar 192000 -ac 2 -f f32le song.f32
// Without it, plays a quiet 440 Hz tone. Set LS50STREAM_MACH_SERVICE to reach a development ls50streamd.

#include <CoreAudio/AudioServerPlugIn.h>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int failures = 0;
#define CHECK(cond, ...) do { if (!(cond)) { printf("FAIL: " __VA_ARGS__); printf("\n"); failures++; } } while (0)
#define GLOB 0x676c6f62

static AudioServerPlugInDriverRef drv;

static OSStatus HostPropertiesChanged(AudioServerPlugInHostRef h, AudioObjectID obj, UInt32 n, const AudioObjectPropertyAddress *a) {
    (void)h; printf("  host: PropertiesChanged obj=%u n=%u sel='%.4s'\n", obj, n, (char *)&(UInt32){CFSwapInt32HostToBig(a[0].mSelector)});
    return 0;
}
static OSStatus HostCopyFromStorage(AudioServerPlugInHostRef h, CFStringRef k, CFPropertyListRef *d) { (void)h; (void)k; *d = NULL; return 0; }
static OSStatus HostWriteToStorage(AudioServerPlugInHostRef h, CFStringRef k, CFPropertyListRef d) {
    (void)h; (void)d; char key[64]; CFStringGetCString(k, key, sizeof key, kCFStringEncodingUTF8); printf("  host: WriteToStorage '%s'\n", key); return 0;
}
static OSStatus HostDeleteFromStorage(AudioServerPlugInHostRef h, CFStringRef k) { (void)h; (void)k; return 0; }
// Like the HAL: perform the requested change right away (IO is stopped in this harness when we switch).
static OSStatus HostRequestConfigChange(AudioServerPlugInHostRef h, AudioObjectID o, UInt64 action, void *info) {
    (void)h;
    printf("  host: RequestDeviceConfigurationChange action=%llu\n", action);
    return (*drv)->PerformDeviceConfigurationChange(drv, o, action, info);
}

static AudioServerPlugInHostInterface gHostInterface = {
    HostPropertiesChanged, HostCopyFromStorage, HostWriteToStorage, HostDeleteFromStorage, HostRequestConfigChange,
};

static OSStatus Get(AudioObjectID obj, AudioObjectPropertySelector sel, AudioObjectPropertyScope scope, void *out, UInt32 size, UInt32 *outSize) {
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    if (!(*drv)->HasProperty(drv, obj, 0, &a)) return -1;
    UInt32 needed = 0;
    OSStatus err = (*drv)->GetPropertyDataSize(drv, obj, 0, &a, 0, NULL, &needed);
    if (err) return err;
    UInt32 got = 0;
    err = (*drv)->GetPropertyData(drv, obj, 0, &a, 0, NULL, size, &got, out);
    if (outSize) *outSize = got;
    return err;
}

static UInt32 GetU32(AudioObjectID obj, AudioObjectPropertySelector sel, AudioObjectPropertyScope scope) {
    UInt32 v = 0xdeadbeef;
    OSStatus err = Get(obj, sel, scope, &v, sizeof v, NULL);
    CHECK(err == 0, "get u32 obj %u sel %.4s -> %d", obj, (char *)&(UInt32){CFSwapInt32HostToBig(sel)}, (int)err);
    return v;
}

static Float64 Rate(void) {
    Float64 r = 0;
    Get(2, kAudioDevicePropertyNominalSampleRate, GLOB, &r, sizeof r, NULL);
    return r;
}

static OSStatus SetRate(Float64 rate) {
    AudioObjectPropertyAddress a = {kAudioDevicePropertyNominalSampleRate, GLOB, 0};
    return (*drv)->SetPropertyData(drv, 2, 0, &a, 0, NULL, sizeof rate, &rate);
}

static UInt32 DeviceCount(void) {
    UInt32 n = 0;
    AudioObjectID ids[2];
    Get(1, kAudioPlugInPropertyDeviceList, GLOB, ids, sizeof ids, &n);
    return n / sizeof(AudioObjectID);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc < 3) { fprintf(stderr, "usage: harness <driver binary> <seconds> [audio.f32 [rate]]\n"); return 2; }
    void *lib = dlopen(argv[1], RTLD_NOW);
    if (!lib) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 1; }
    void *(*create)(CFAllocatorRef, CFUUIDRef) = dlsym(lib, "LS50Stream_Create");
    drv = create(NULL, kAudioServerPlugInTypeUUID);
    CHECK(drv != NULL, "factory");
    CHECK((*drv)->Initialize(drv, &gHostInterface) == 0, "initialize");

    printf("Properties:\n");
    CHECK(GetU32(2, kAudioObjectPropertyClass, GLOB) == kAudioDeviceClassID, "device class");
    CHECK(GetU32(3, kAudioStreamPropertyDirection, GLOB) == 0, "stream is output");
    AudioStreamBasicDescription fmt;
    CHECK(Get(3, kAudioStreamPropertyVirtualFormat, GLOB, &fmt, sizeof fmt, NULL) == 0, "format");
    printf("  format         %.0f Hz, %u ch, %u bit float\n", fmt.mSampleRate, fmt.mChannelsPerFrame, fmt.mBitsPerChannel);
    CHECK(fmt.mChannelsPerFrame == 2 && fmt.mSampleRate == 192000, "stereo, 192 kHz by default");
    AudioValueRange rates[4]; UInt32 n = 0;
    Get(2, kAudioDevicePropertyAvailableNominalSampleRates, GLOB, rates, sizeof rates, &n);
    CHECK(n == 3 * sizeof(AudioValueRange) && rates[0].mMinimum == 48000 && rates[2].mMinimum == 192000, "rates 48/96/192 kHz");
    AudioStreamRangedDescription formats[4];
    Get(3, kAudioStreamPropertyAvailableVirtualFormats, GLOB, formats, sizeof formats, &n);
    CHECK(n == 3 * sizeof(AudioStreamRangedDescription) && formats[1].mFormat.mSampleRate == 96000, "three stream formats");
    UInt32 stereo[2]; Get(2, kAudioDevicePropertyPreferredChannelsForStereo, GLOB, stereo, sizeof stereo, NULL);
    CHECK(stereo[0] == 1 && stereo[1] == 2, "stereo pair");

    printf("Rate switching:\n");
    CHECK(SetRate(96000) == 0 && Rate() == 96000, "switch to 96 kHz");
    Get(3, kAudioStreamPropertyVirtualFormat, GLOB, &fmt, sizeof fmt, NULL);
    CHECK(fmt.mSampleRate == 96000, "stream format follows the rate");
    CHECK(SetRate(44100) != 0 && Rate() == 96000, "reject 44.1 kHz");

    int seconds = atoi(argv[2]);
    const char *audioPath = argc > 3 ? argv[3] : NULL;
    Float64 rate = argc > 4 ? atof(argv[4]) : 192000;
    CHECK(SetRate(rate) == 0 && Rate() == rate, "switch to %.0f Hz for playback", rate);

    printf("Waiting for ls50streamd to publish the device (speaker found)...\n");
    for (int i = 0; i < 120 && DeviceCount() == 0; i++) usleep(250000);
    CHECK(DeviceCount() == 1, "device published");
    if (failures) { printf("FAILED (%d failures)\n", failures); return 1; }

    float *audio = NULL; size_t audioFrames = 0;
    if (audioPath) {
        FILE *f = fopen(audioPath, "rb");
        if (!f) { perror(audioPath); return 1; }
        fseek(f, 0, SEEK_END); long bytes = ftell(f); fseek(f, 0, SEEK_SET);
        audio = malloc((size_t)bytes);
        audioFrames = fread(audio, 1, (size_t)bytes, f) / (2 * sizeof(float));
        fclose(f);
        printf("Playing %s (%.1f s at %.0f Hz) for %d s\n", audioPath, audioFrames / rate, rate, seconds);
    } else {
        printf("Playing a quiet 440 Hz tone for %d s\n", seconds);
    }

    CHECK((*drv)->StartIO(drv, 2, 0) == 0, "StartIO");
    mach_timebase_info_data_t tb; mach_timebase_info(&tb);
    double ticksPerFrame = (1e9 * tb.denom / tb.numer) / rate;
    const UInt32 io = 1024;
    float *buf = calloc(io * 2, sizeof(float));
    Float64 zs; UInt64 zh, seed;
    Float64 nextOut = 0;
    size_t played = 0;
    UInt64 end = mach_absolute_time() + (UInt64)(seconds * rate * ticksPerFrame);
    while (mach_absolute_time() < end) {
        (*drv)->GetZeroTimeStamp(drv, 2, 0, &zs, &zh, &seed);
        Float64 now = zs + (mach_absolute_time() - zh) / ticksPerFrame;
        // Render ahead like the HAL does (two IO periods of lead for this non-real-time loop)
        if (nextOut == 0) nextOut = floor(now) + 2 * io;
        if (now + 2 * io >= nextOut) {
            for (UInt32 fr = 0; fr < io; fr++) {
                if (audio) {
                    size_t src = (played + fr) % audioFrames;
                    buf[fr * 2] = audio[src * 2];
                    buf[fr * 2 + 1] = audio[src * 2 + 1];
                } else {
                    float s = 0.05f * (float)sin(2 * M_PI * 440 * (nextOut + fr) / rate);
                    buf[fr * 2] = buf[fr * 2 + 1] = s;
                }
            }
            played += io;
            AudioServerPlugInIOCycleInfo cycle = {0};
            cycle.mOutputTime.mSampleTime = nextOut;
            cycle.mNominalIOBufferFrameSize = io;
            (*drv)->DoIOOperation(drv, 2, 3, 0, kAudioServerPlugInIOOperationWriteMix, io, &cycle, buf, NULL);
            nextOut += io;
        }
        usleep(1000);
    }
    (*drv)->StopIO(drv, 2, 0);
    printf("%s (%d failures)\n", failures ? "FAILED" : "PASSED", failures);
    return failures ? 1 : 0;
}
