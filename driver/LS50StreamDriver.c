// LS50Stream.driver: a CoreAudio AudioServerPlugIn providing a stereo output device for the KEF speakers.
//
// The device's output mix is written into a shared-memory ring served by ls50streamd (see ls50stream_shm.h),
// which streams it to the speakers over DLNA. Objects: plug-in, one device, one stereo output stream,
// a master volume and a master mute control. The device runs on its own clock (mach time at the nominal
// rate: 48, 96 or 192 kHz, set in Audio MIDI Setup). It is only published while ls50streamd sees the
// speakers on the network.

#include <CoreAudio/AudioServerPlugIn.h>
#include <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/mman.h>
#include <xpc/xpc.h>

#include "ls50stream_shm.h"

enum {
    kObjectID_PlugIn = kAudioObjectPlugInObject,
    kObjectID_Device = 2,
    kObjectID_Stream = 3,
    kObjectID_Volume = 4,
    kObjectID_Mute = 5,
};

#define kDeviceName CFSTR("KEF LS50W")
#define kManufacturer CFSTR("LS50Stream")
#define kDeviceUID "LS50Stream_UID"
#define kDeviceModelUID CFSTR("LS50Stream_ModelUID")
#define kChannels LS50STREAM_CHANNELS
#define kZeroTimeStampPeriod LS50STREAM_RING_FRAMES
#define kMinDB (-64.0f)
#define kStorageSampleRate CFSTR("sample rate")

// Rates the speakers accept; the highest has the least delay (they buffer a fixed amount of data).
static const Float64 kSampleRates[] = {48000.0, 96000.0, 192000.0};
#define kSampleRateCount (sizeof kSampleRates / sizeof kSampleRates[0])
#define kDefaultSampleRate 48000.0 // safe on Wi-Fi; 96/192 kHz lower the delay on a cable

// ---------------------------------------------------------------------------------------------
// State

static pthread_mutex_t gStateMutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t gTimeMutex = PTHREAD_MUTEX_INITIALIZER;
static UInt32 gRefCount = 0;
static AudioServerPlugInHostRef gHost = NULL;

// Nominal rate; changed only in PerformDeviceConfigurationChange (IO stopped). Guarded by gStateMutex.
static Float64 gSampleRate = kDefaultSampleRate;

// Device timeline (see GetZeroTimeStamp); guarded by gStateMutex / gTimeMutex
static UInt32 gIOClients = 0;
static UInt32 gGeneration = 0;
static Float64 gHostTicksPerFrame = 0;
static UInt64 gAnchorHostTime = 0;
static UInt64 gNumberTimeStamps = 0;

// Controls (read on the IO thread)
static _Atomic float gVolumeScalar = 1.0f;
static _Atomic UInt32 gMute = 0;

// Shared memory from ls50streamd
static _Atomic(LS50StreamShmHeader *) gShm = NULL;
static dispatch_queue_t gQueue = NULL;
static xpc_connection_t gConnection = NULL;
static bool gAttachScheduled = false;
// Published only while ls50streamd is attached and sees the speakers: otherwise macOS moves audio to
// another output, as if a sound card were unplugged.
static _Atomic bool gDevicePresent = false;
static dispatch_source_t gPresenceTimer = NULL;
// Device latency last seen in shared memory (ls50streamd sets it per sample rate)
static UInt32 gLatencyFrames = 0;

static bool IsSupportedRate(Float64 rate) {
    for (UInt32 i = 0; i < kSampleRateCount; i++) {
        if (kSampleRates[i] == rate) return true;
    }
    return false;
}

static Float64 TicksPerFrame(Float64 rate) {
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    return (1.0e9 * tb.denom / tb.numer) / rate;
}

static AudioStreamBasicDescription FormatAt(Float64 rate) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = rate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBytesPerPacket = 4 * kChannels;
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = 4 * kChannels;
    f.mChannelsPerFrame = kChannels;
    f.mBitsPerChannel = 32;
    return f;
}

static Float64 CurrentRate(void) {
    pthread_mutex_lock(&gStateMutex);
    Float64 rate = gSampleRate;
    pthread_mutex_unlock(&gStateMutex);
    return rate;
}

static float ScalarToDB(float scalar) {
    // Perceptual curve: gain = scalar^2
    if (scalar <= 0.0f) return kMinDB;
    float db = 40.0f * log10f(scalar);
    return db < kMinDB ? kMinDB : (db > 0.0f ? 0.0f : db);
}

static float DBToScalar(float db) {
    if (db <= kMinDB) return 0.0f;
    if (db >= 0.0f) return 1.0f;
    return powf(10.0f, db / 40.0f);
}

static void LoadSampleRate(void) {
    CFPropertyListRef value = NULL;
    if (gHost->CopyFromStorage(gHost, kStorageSampleRate, &value) == noErr && value) {
        Float64 rate = 0;
        if (CFGetTypeID(value) == CFNumberGetTypeID() && CFNumberGetValue(value, kCFNumberFloat64Type, &rate) &&
            IsSupportedRate(rate)) {
            gSampleRate = rate;
        }
        CFRelease(value);
    }
}

static void SaveSampleRate(Float64 rate) {
    CFNumberRef number = CFNumberCreate(NULL, kCFNumberFloat64Type, &rate);
    gHost->WriteToStorage(gHost, kStorageSampleRate, number);
    CFRelease(number);
}

// ---------------------------------------------------------------------------------------------
// Connection to ls50streamd

static void PublishTimeline(LS50StreamShmHeader *h) {
    // Caller holds gStateMutex
    __atomic_store_n(&h->io_running, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&h->write_end, 0, __ATOMIC_RELAXED);
    __atomic_store_n(&h->sample_rate, (uint32_t)gSampleRate, __ATOMIC_RELAXED);
    h->anchor_host_time = gAnchorHostTime;
    h->host_ticks_per_frame = gHostTicksPerFrame;
    __atomic_store_n(&h->generation, gGeneration, __ATOMIC_RELEASE);
    __atomic_store_n(&h->io_running, gIOClients > 0 ? 1 : 0, __ATOMIC_RELEASE);
}

static void Attach(void);

static void UpdatePresence(void) {
    // On gQueue
    LS50StreamShmHeader *h = atomic_load(&gShm);
    bool present = h && !__atomic_load_n(&h->device_hidden, __ATOMIC_ACQUIRE);
    if (present == gDevicePresent) return;
    gDevicePresent = present;
    AudioObjectPropertyAddress changed[] = {
        {kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
    };
    gHost->PropertiesChanged(gHost, kObjectID_PlugIn, 2, changed);
}

static void UpdateLatency(void) {
    // On gQueue: tell CoreAudio (and players syncing video to us) when the latency changes
    LS50StreamShmHeader *h = atomic_load(&gShm);
    UInt32 latency = h ? __atomic_load_n(&h->latency_frames, __ATOMIC_RELAXED) : 0;
    if (latency == gLatencyFrames || !gDevicePresent) return;
    gLatencyFrames = latency;
    AudioObjectPropertyAddress changed = {kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
    gHost->PropertiesChanged(gHost, kObjectID_Device, 1, &changed);
}

static void ScheduleAttach(double seconds) {
    // On gQueue
    if (gAttachScheduled) return;
    gAttachScheduled = true;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(seconds * NSEC_PER_SEC)), gQueue, ^{
        gAttachScheduled = false;
        Attach();
    });
}

static void ResetConnection(void) {
    if (gConnection) {
        xpc_connection_cancel(gConnection);
        xpc_release(gConnection);
        gConnection = NULL;
    }
}

static void Detach(void) {
    // On gQueue: ls50streamd went away, so nothing would play; hide the device
    LS50StreamShmHeader *old = atomic_exchange(&gShm, NULL);
    if (old) {
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), gQueue, ^{
            munmap(old, LS50STREAM_SHM_SIZE);
        });
    }
    UpdatePresence();
}

static void Attach(void) {
    // On gQueue
    if (!gConnection) {
        gConnection = xpc_connection_create_mach_service(LS50STREAM_MACH_SERVICE, gQueue, 0);
        xpc_connection_t connection = gConnection;
        xpc_connection_set_event_handler(connection, ^(xpc_object_t event) {
            if (connection != gConnection) return;
            if (event == XPC_ERROR_CONNECTION_INTERRUPTED) {
                Detach();
                ScheduleAttach(1.0); // ls50streamd restarted: fetch its new region
            } else if (event == XPC_ERROR_CONNECTION_INVALID) {
                Detach();
                ResetConnection();
                ScheduleAttach(3.0); // ls50streamd not installed or not running yet
            }
        });
        xpc_connection_resume(connection);
    }
    xpc_object_t message = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_string(message, "op", "attach");
    xpc_connection_send_message_with_reply(gConnection, message, gQueue, ^(xpc_object_t reply) {
        if (xpc_get_type(reply) != XPC_TYPE_DICTIONARY) {
            ScheduleAttach(3.0);
            return;
        }
        xpc_object_t shmem = xpc_dictionary_get_value(reply, "shm");
        void *address = NULL;
        size_t size = shmem ? xpc_shmem_map(shmem, &address) : 0;
        LS50StreamShmHeader *h = (LS50StreamShmHeader *)address;
        if (!h || size < LS50STREAM_SHM_SIZE || h->magic != LS50STREAM_SHM_MAGIC || h->version != LS50STREAM_SHM_VERSION ||
            h->channels != kChannels || h->ring_frames != LS50STREAM_RING_FRAMES) {
            if (address) munmap(address, size);
            ScheduleAttach(10.0);
            return;
        }
        pthread_mutex_lock(&gStateMutex);
        PublishTimeline(h);
        __atomic_store_n(&h->attached, 1, __ATOMIC_RELEASE);
        LS50StreamShmHeader *old = atomic_exchange(&gShm, h);
        pthread_mutex_unlock(&gStateMutex);
        if (old) {
            // The IO thread may still be writing to the old region for a moment
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), gQueue, ^{
                munmap(old, LS50STREAM_SHM_SIZE);
            });
        }
        UpdatePresence();
    });
    xpc_release(message);
}

// ---------------------------------------------------------------------------------------------
// Property helpers

#define REQUIRE_SIZE(needed)                                   \
    do {                                                       \
        if (inDataSize < (needed)) return kAudioHardwareBadPropertySizeError; \
        *outDataSize = (needed);                               \
    } while (0)

static bool IsOutputOrGlobal(AudioObjectPropertyScope scope) {
    return scope == kAudioObjectPropertyScopeGlobal || scope == kAudioObjectPropertyScopeOutput;
}

static OSStatus WriteIDs(const AudioObjectID *ids, UInt32 count, UInt32 inDataSize, UInt32 *outDataSize, void *outData) {
    // Arrays: return as many items as fit
    UInt32 n = inDataSize / sizeof(AudioObjectID);
    if (n > count) n = count;
    memcpy(outData, ids, n * sizeof(AudioObjectID));
    *outDataSize = n * sizeof(AudioObjectID);
    return noErr;
}

// A stereo layout: Left/Right
#define kLayoutSize (offsetof(AudioChannelLayout, mChannelDescriptions) + kChannels * sizeof(AudioChannelDescription))

// Property sizes; also used for HasProperty. Returns kAudioHardwareUnknownPropertyError if absent.
static OSStatus PropertySize(AudioObjectID obj, const AudioObjectPropertyAddress *a, UInt32 *outSize) {
    switch (obj) {
    case kObjectID_PlugIn:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass: *outSize = sizeof(AudioClassID); return noErr;
        case kAudioObjectPropertyOwner:
        case kAudioPlugInPropertyTranslateUIDToDevice: *outSize = sizeof(AudioObjectID); return noErr;
        case kAudioObjectPropertyManufacturer:
        case kAudioPlugInPropertyResourceBundle: *outSize = sizeof(CFStringRef); return noErr;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: *outSize = (gDevicePresent ? 1 : 0) * sizeof(AudioObjectID); return noErr;
        }
        break;
    case kObjectID_Device:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass: *outSize = sizeof(AudioClassID); return noErr;
        case kAudioObjectPropertyOwner: *outSize = sizeof(AudioObjectID); return noErr;
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID: *outSize = sizeof(CFStringRef); return noErr;
        case kAudioObjectPropertyOwnedObjects:
            *outSize = (IsOutputOrGlobal(a->mScope) ? 3 : 0) * sizeof(AudioObjectID);
            return noErr;
        case kAudioDevicePropertyStreams:
            *outSize = (IsOutputOrGlobal(a->mScope) ? 1 : 0) * sizeof(AudioObjectID);
            return noErr;
        case kAudioObjectPropertyControlList: *outSize = 2 * sizeof(AudioObjectID); return noErr;
        case kAudioDevicePropertyRelatedDevices: *outSize = sizeof(AudioObjectID); return noErr;
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyZeroTimeStampPeriod: *outSize = sizeof(UInt32); return noErr;
        case kAudioDevicePropertyNominalSampleRate: *outSize = sizeof(Float64); return noErr;
        case kAudioDevicePropertyAvailableNominalSampleRates: *outSize = kSampleRateCount * sizeof(AudioValueRange); return noErr;
        case kAudioDevicePropertyPreferredChannelsForStereo: *outSize = 2 * sizeof(UInt32); return noErr;
        case kAudioDevicePropertyPreferredChannelLayout: *outSize = kLayoutSize; return noErr;
        }
        break;
    case kObjectID_Stream:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass: *outSize = sizeof(AudioClassID); return noErr;
        case kAudioObjectPropertyOwner: *outSize = sizeof(AudioObjectID); return noErr;
        case kAudioObjectPropertyOwnedObjects: *outSize = 0; return noErr;
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency: *outSize = sizeof(UInt32); return noErr;
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat: *outSize = sizeof(AudioStreamBasicDescription); return noErr;
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: *outSize = kSampleRateCount * sizeof(AudioStreamRangedDescription); return noErr;
        }
        break;
    case kObjectID_Volume:
    case kObjectID_Mute:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass: *outSize = sizeof(AudioClassID); return noErr;
        case kAudioObjectPropertyOwner: *outSize = sizeof(AudioObjectID); return noErr;
        case kAudioObjectPropertyOwnedObjects: *outSize = 0; return noErr;
        case kAudioControlPropertyScope: *outSize = sizeof(AudioObjectPropertyScope); return noErr;
        case kAudioControlPropertyElement: *outSize = sizeof(AudioObjectPropertyElement); return noErr;
        }
        if (obj == kObjectID_Volume) {
            switch (a->mSelector) {
            case kAudioLevelControlPropertyScalarValue:
            case kAudioLevelControlPropertyDecibelValue:
            case kAudioLevelControlPropertyConvertScalarToDecibels:
            case kAudioLevelControlPropertyConvertDecibelsToScalar: *outSize = sizeof(Float32); return noErr;
            case kAudioLevelControlPropertyDecibelRange: *outSize = sizeof(AudioValueRange); return noErr;
            }
        } else if (a->mSelector == kAudioBooleanControlPropertyValue) {
            *outSize = sizeof(UInt32);
            return noErr;
        }
        break;
    default:
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareUnknownPropertyError;
}

// ---------------------------------------------------------------------------------------------
// Driver interface

static HRESULT LS50Stream_QueryInterface(void *inDriver, REFIID inUUID, LPVOID *outInterface);
static ULONG LS50Stream_AddRef(void *inDriver);
static ULONG LS50Stream_Release(void *inDriver);
static OSStatus LS50Stream_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost);
static OSStatus LS50Stream_CreateDevice(AudioServerPlugInDriverRef, CFDictionaryRef, const AudioServerPlugInClientInfo *, AudioObjectID *);
static OSStatus LS50Stream_DestroyDevice(AudioServerPlugInDriverRef, AudioObjectID);
static OSStatus LS50Stream_AddDeviceClient(AudioServerPlugInDriverRef, AudioObjectID, const AudioServerPlugInClientInfo *);
static OSStatus LS50Stream_RemoveDeviceClient(AudioServerPlugInDriverRef, AudioObjectID, const AudioServerPlugInClientInfo *);
static OSStatus LS50Stream_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID, UInt64, void *);
static OSStatus LS50Stream_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID, UInt64, void *);
static Boolean LS50Stream_HasProperty(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *);
static OSStatus LS50Stream_IsPropertySettable(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, Boolean *);
static OSStatus LS50Stream_GetPropertyDataSize(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32 *);
static OSStatus LS50Stream_GetPropertyData(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32, UInt32 *, void *);
static OSStatus LS50Stream_SetPropertyData(AudioServerPlugInDriverRef, AudioObjectID, pid_t, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32, const void *);
static OSStatus LS50Stream_StartIO(AudioServerPlugInDriverRef, AudioObjectID, UInt32);
static OSStatus LS50Stream_StopIO(AudioServerPlugInDriverRef, AudioObjectID, UInt32);
static OSStatus LS50Stream_GetZeroTimeStamp(AudioServerPlugInDriverRef, AudioObjectID, UInt32, Float64 *, UInt64 *, UInt64 *);
static OSStatus LS50Stream_WillDoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, Boolean *, Boolean *);
static OSStatus LS50Stream_BeginIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *);
static OSStatus LS50Stream_DoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *, void *, void *);
static OSStatus LS50Stream_EndIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32, const AudioServerPlugInIOCycleInfo *);

static AudioServerPlugInDriverInterface gInterface = {
    NULL,
    LS50Stream_QueryInterface,
    LS50Stream_AddRef,
    LS50Stream_Release,
    LS50Stream_Initialize,
    LS50Stream_CreateDevice,
    LS50Stream_DestroyDevice,
    LS50Stream_AddDeviceClient,
    LS50Stream_RemoveDeviceClient,
    LS50Stream_PerformDeviceConfigurationChange,
    LS50Stream_AbortDeviceConfigurationChange,
    LS50Stream_HasProperty,
    LS50Stream_IsPropertySettable,
    LS50Stream_GetPropertyDataSize,
    LS50Stream_GetPropertyData,
    LS50Stream_SetPropertyData,
    LS50Stream_StartIO,
    LS50Stream_StopIO,
    LS50Stream_GetZeroTimeStamp,
    LS50Stream_WillDoIOOperation,
    LS50Stream_BeginIOOperation,
    LS50Stream_DoIOOperation,
    LS50Stream_EndIOOperation,
};
static AudioServerPlugInDriverInterface *gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriverRef = &gInterfacePtr;

// Factory named in Info.plist (CFPlugInFactories)
void *LS50Stream_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID) {
    (void)inAllocator;
    return CFEqual(inRequestedTypeUUID, kAudioServerPlugInTypeUUID) ? gDriverRef : NULL;
}

static HRESULT LS50Stream_QueryInterface(void *inDriver, REFIID inUUID, LPVOID *outInterface) {
    if (inDriver != gDriverRef || !outInterface) return kAudioHardwareBadObjectError;
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(NULL, inUUID);
    HRESULT result = E_NOINTERFACE;
    if (CFEqual(requested, IUnknownUUID) || CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID)) {
        pthread_mutex_lock(&gStateMutex);
        ++gRefCount;
        pthread_mutex_unlock(&gStateMutex);
        *outInterface = gDriverRef;
        result = S_OK;
    }
    CFRelease(requested);
    return result;
}

static ULONG LS50Stream_AddRef(void *inDriver) {
    if (inDriver != gDriverRef) return 0;
    pthread_mutex_lock(&gStateMutex);
    ULONG count = ++gRefCount;
    pthread_mutex_unlock(&gStateMutex);
    return count;
}

static ULONG LS50Stream_Release(void *inDriver) {
    if (inDriver != gDriverRef) return 0;
    pthread_mutex_lock(&gStateMutex);
    ULONG count = gRefCount > 0 ? --gRefCount : 0;
    pthread_mutex_unlock(&gStateMutex);
    return count;
}

static OSStatus LS50Stream_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost) {
    if (inDriver != gDriverRef) return kAudioHardwareBadObjectError;
    gHost = inHost;
    LoadSampleRate();
    gHostTicksPerFrame = TicksPerFrame(gSampleRate);
    gQueue = dispatch_queue_create("io.github.kaihagseth.ls50stream.driver", DISPATCH_QUEUE_SERIAL);
    dispatch_async(gQueue, ^{ Attach(); });
    gPresenceTimer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, gQueue);
    dispatch_source_set_timer(gPresenceTimer, dispatch_time(DISPATCH_TIME_NOW, 0), 250 * NSEC_PER_MSEC, 50 * NSEC_PER_MSEC);
    dispatch_source_set_event_handler(gPresenceTimer, ^{ UpdatePresence(); UpdateLatency(); });
    dispatch_resume(gPresenceTimer);
    return noErr;
}

static OSStatus LS50Stream_CreateDevice(AudioServerPlugInDriverRef d, CFDictionaryRef desc, const AudioServerPlugInClientInfo *c, AudioObjectID *out) {
    (void)d; (void)desc; (void)c; (void)out;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus LS50Stream_DestroyDevice(AudioServerPlugInDriverRef d, AudioObjectID id) {
    (void)d; (void)id;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus LS50Stream_AddDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID id, const AudioServerPlugInClientInfo *c) {
    (void)c;
    return (d == gDriverRef && id == kObjectID_Device) ? noErr : kAudioHardwareBadObjectError;
}

static OSStatus LS50Stream_RemoveDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID id, const AudioServerPlugInClientInfo *c) {
    (void)c;
    return (d == gDriverRef && id == kObjectID_Device) ? noErr : kAudioHardwareBadObjectError;
}

// The host calls this (with IO stopped) after we asked for a rate change; `action` is the new rate.
static OSStatus LS50Stream_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID id, UInt64 action, void *info) {
    (void)info;
    if (d != gDriverRef || id != kObjectID_Device) return kAudioHardwareBadObjectError;
    Float64 rate = (Float64)action;
    if (!IsSupportedRate(rate)) return kAudioHardwareIllegalOperationError;
    pthread_mutex_lock(&gStateMutex);
    gSampleRate = rate;
    pthread_mutex_lock(&gTimeMutex);
    gHostTicksPerFrame = TicksPerFrame(rate);
    gNumberTimeStamps = 0;
    gAnchorHostTime = mach_absolute_time();
    pthread_mutex_unlock(&gTimeMutex);
    ++gGeneration;
    LS50StreamShmHeader *h = atomic_load(&gShm);
    if (h) PublishTimeline(h);
    pthread_mutex_unlock(&gStateMutex);
    SaveSampleRate(rate);
    return noErr;
}

static OSStatus LS50Stream_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID id, UInt64 action, void *info) {
    (void)action; (void)info;
    return (d == gDriverRef && id == kObjectID_Device) ? noErr : kAudioHardwareBadObjectError;
}

static Boolean LS50Stream_HasProperty(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a) {
    (void)pid;
    UInt32 size;
    return d == gDriverRef && a && PropertySize(obj, a, &size) == noErr;
}

static OSStatus LS50Stream_IsPropertySettable(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a, Boolean *outSettable) {
    (void)pid;
    if (d != gDriverRef || !a || !outSettable) return kAudioHardwareIllegalOperationError;
    UInt32 size;
    OSStatus err = PropertySize(obj, a, &size);
    if (err) return err;
    switch (a->mSelector) {
    case kAudioStreamPropertyIsActive:
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat:
        *outSettable = obj == kObjectID_Stream;
        break;
    case kAudioDevicePropertyNominalSampleRate:
        *outSettable = obj == kObjectID_Device;
        break;
    case kAudioLevelControlPropertyScalarValue:
    case kAudioLevelControlPropertyDecibelValue:
        *outSettable = obj == kObjectID_Volume;
        break;
    case kAudioBooleanControlPropertyValue:
        *outSettable = obj == kObjectID_Mute;
        break;
    default:
        *outSettable = false;
    }
    return noErr;
}

static OSStatus LS50Stream_GetPropertyDataSize(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a, UInt32 qSize, const void *q, UInt32 *outSize) {
    (void)pid; (void)qSize; (void)q;
    if (d != gDriverRef || !a || !outSize) return kAudioHardwareIllegalOperationError;
    return PropertySize(obj, a, outSize);
}

static OSStatus LS50Stream_GetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a,
                                         UInt32 qSize, const void *q, UInt32 inDataSize, UInt32 *outDataSize, void *outData) {
    (void)pid;
    if (d != gDriverRef || !a || !outDataSize || !outData) return kAudioHardwareIllegalOperationError;
    UInt32 size;
    OSStatus err = PropertySize(obj, a, &size);
    if (err) return err;

    switch (obj) {
    case kObjectID_PlugIn:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass: REQUIRE_SIZE(sizeof(AudioClassID)); *(AudioClassID *)outData = kAudioObjectClassID; return noErr;
        case kAudioObjectPropertyClass: REQUIRE_SIZE(sizeof(AudioClassID)); *(AudioClassID *)outData = kAudioPlugInClassID; return noErr;
        case kAudioObjectPropertyOwner: REQUIRE_SIZE(sizeof(AudioObjectID)); *(AudioObjectID *)outData = kAudioObjectUnknown; return noErr;
        case kAudioObjectPropertyManufacturer: REQUIRE_SIZE(sizeof(CFStringRef)); *(CFStringRef *)outData = kManufacturer; return noErr;
        case kAudioPlugInPropertyResourceBundle: REQUIRE_SIZE(sizeof(CFStringRef)); *(CFStringRef *)outData = CFSTR(""); return noErr;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: {
            AudioObjectID ids[] = {kObjectID_Device};
            return WriteIDs(ids, gDevicePresent ? 1 : 0, inDataSize, outDataSize, outData);
        }
        case kAudioPlugInPropertyTranslateUIDToDevice: {
            REQUIRE_SIZE(sizeof(AudioObjectID));
            if (qSize != sizeof(CFStringRef) || !q) return kAudioHardwareBadPropertySizeError;
            CFStringRef uid = *(const CFStringRef *)q;
            *(AudioObjectID *)outData = (gDevicePresent && uid && CFEqual(uid, CFSTR(kDeviceUID))) ? kObjectID_Device : kAudioObjectUnknown;
            return noErr;
        }
        }
        break;

    case kObjectID_Device:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass: REQUIRE_SIZE(sizeof(AudioClassID)); *(AudioClassID *)outData = kAudioObjectClassID; return noErr;
        case kAudioObjectPropertyClass: REQUIRE_SIZE(sizeof(AudioClassID)); *(AudioClassID *)outData = kAudioDeviceClassID; return noErr;
        case kAudioObjectPropertyOwner: REQUIRE_SIZE(sizeof(AudioObjectID)); *(AudioObjectID *)outData = kObjectID_PlugIn; return noErr;
        case kAudioObjectPropertyName: REQUIRE_SIZE(sizeof(CFStringRef)); *(CFStringRef *)outData = kDeviceName; return noErr;
        case kAudioObjectPropertyManufacturer: REQUIRE_SIZE(sizeof(CFStringRef)); *(CFStringRef *)outData = kManufacturer; return noErr;
        case kAudioDevicePropertyDeviceUID: REQUIRE_SIZE(sizeof(CFStringRef)); *(CFStringRef *)outData = CFSTR(kDeviceUID); return noErr;
        case kAudioDevicePropertyModelUID: REQUIRE_SIZE(sizeof(CFStringRef)); *(CFStringRef *)outData = kDeviceModelUID; return noErr;
        case kAudioObjectPropertyOwnedObjects: {
            AudioObjectID ids[] = {kObjectID_Stream, kObjectID_Volume, kObjectID_Mute};
            return WriteIDs(ids, IsOutputOrGlobal(a->mScope) ? 3 : 0, inDataSize, outDataSize, outData);
        }
        case kAudioDevicePropertyStreams: {
            AudioObjectID ids[] = {kObjectID_Stream};
            return WriteIDs(ids, IsOutputOrGlobal(a->mScope) ? 1 : 0, inDataSize, outDataSize, outData);
        }
        case kAudioObjectPropertyControlList: {
            AudioObjectID ids[] = {kObjectID_Volume, kObjectID_Mute};
            return WriteIDs(ids, 2, inDataSize, outDataSize, outData);
        }
        case kAudioDevicePropertyRelatedDevices: {
            AudioObjectID ids[] = {kObjectID_Device};
            return WriteIDs(ids, 1, inDataSize, outDataSize, outData);
        }
        case kAudioDevicePropertyTransportType: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = kAudioDeviceTransportTypeVirtual; return noErr;
        case kAudioDevicePropertyClockDomain: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertyDeviceIsAlive: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 1; return noErr;
        case kAudioDevicePropertyDeviceIsRunning:
            REQUIRE_SIZE(sizeof(UInt32));
            pthread_mutex_lock(&gStateMutex);
            *(UInt32 *)outData = gIOClients > 0;
            pthread_mutex_unlock(&gStateMutex);
            return noErr;
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
            REQUIRE_SIZE(sizeof(UInt32));
            *(UInt32 *)outData = a->mScope != kAudioObjectPropertyScopeInput;
            return noErr;
        case kAudioDevicePropertyLatency: {
            REQUIRE_SIZE(sizeof(UInt32));
            LS50StreamShmHeader *h = atomic_load(&gShm);
            *(UInt32 *)outData = (h && a->mScope != kAudioObjectPropertyScopeInput) ? __atomic_load_n(&h->latency_frames, __ATOMIC_RELAXED) : 0;
            return noErr;
        }
        case kAudioDevicePropertySafetyOffset: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertyIsHidden: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 0; return noErr;
        case kAudioDevicePropertyZeroTimeStampPeriod: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = kZeroTimeStampPeriod; return noErr;
        case kAudioDevicePropertyNominalSampleRate: REQUIRE_SIZE(sizeof(Float64)); *(Float64 *)outData = CurrentRate(); return noErr;
        case kAudioDevicePropertyAvailableNominalSampleRates: {
            UInt32 n = inDataSize / sizeof(AudioValueRange);
            if (n > kSampleRateCount) n = kSampleRateCount;
            for (UInt32 i = 0; i < n; i++) {
                AudioValueRange r = {kSampleRates[i], kSampleRates[i]};
                ((AudioValueRange *)outData)[i] = r;
            }
            *outDataSize = n * sizeof(AudioValueRange);
            return noErr;
        }
        case kAudioDevicePropertyPreferredChannelsForStereo: {
            REQUIRE_SIZE(2 * sizeof(UInt32));
            UInt32 pair[2] = {1, 2};
            memcpy(outData, pair, sizeof pair);
            return noErr;
        }
        case kAudioDevicePropertyPreferredChannelLayout: {
            REQUIRE_SIZE(kLayoutSize);
            AudioChannelLayout *layout = outData;
            memset(layout, 0, kLayoutSize);
            layout->mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
            layout->mNumberChannelDescriptions = kChannels;
            layout->mChannelDescriptions[0].mChannelLabel = kAudioChannelLabel_Left;
            layout->mChannelDescriptions[1].mChannelLabel = kAudioChannelLabel_Right;
            return noErr;
        }
        }
        break;

    case kObjectID_Stream:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass: REQUIRE_SIZE(sizeof(AudioClassID)); *(AudioClassID *)outData = kAudioObjectClassID; return noErr;
        case kAudioObjectPropertyClass: REQUIRE_SIZE(sizeof(AudioClassID)); *(AudioClassID *)outData = kAudioStreamClassID; return noErr;
        case kAudioObjectPropertyOwner: REQUIRE_SIZE(sizeof(AudioObjectID)); *(AudioObjectID *)outData = kObjectID_Device; return noErr;
        case kAudioObjectPropertyOwnedObjects: *outDataSize = 0; return noErr;
        case kAudioStreamPropertyIsActive: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 1; return noErr;
        case kAudioStreamPropertyDirection: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 0; return noErr; // output
        case kAudioStreamPropertyTerminalType: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = kAudioStreamTerminalTypeSpeaker; return noErr;
        case kAudioStreamPropertyStartingChannel: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 1; return noErr;
        case kAudioStreamPropertyLatency: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = 0; return noErr;
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            REQUIRE_SIZE(sizeof(AudioStreamBasicDescription));
            *(AudioStreamBasicDescription *)outData = FormatAt(CurrentRate());
            return noErr;
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: {
            UInt32 n = inDataSize / sizeof(AudioStreamRangedDescription);
            if (n > kSampleRateCount) n = kSampleRateCount;
            for (UInt32 i = 0; i < n; i++) {
                AudioStreamRangedDescription r;
                r.mFormat = FormatAt(kSampleRates[i]);
                r.mSampleRateRange.mMinimum = kSampleRates[i];
                r.mSampleRateRange.mMaximum = kSampleRates[i];
                ((AudioStreamRangedDescription *)outData)[i] = r;
            }
            *outDataSize = n * sizeof(AudioStreamRangedDescription);
            return noErr;
        }
        }
        break;

    case kObjectID_Volume:
    case kObjectID_Mute:
        switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
            REQUIRE_SIZE(sizeof(AudioClassID));
            *(AudioClassID *)outData = obj == kObjectID_Volume ? kAudioLevelControlClassID : kAudioBooleanControlClassID;
            return noErr;
        case kAudioObjectPropertyClass:
            REQUIRE_SIZE(sizeof(AudioClassID));
            *(AudioClassID *)outData = obj == kObjectID_Volume ? kAudioVolumeControlClassID : kAudioMuteControlClassID;
            return noErr;
        case kAudioObjectPropertyOwner: REQUIRE_SIZE(sizeof(AudioObjectID)); *(AudioObjectID *)outData = kObjectID_Device; return noErr;
        case kAudioObjectPropertyOwnedObjects: *outDataSize = 0; return noErr;
        case kAudioControlPropertyScope: REQUIRE_SIZE(sizeof(AudioObjectPropertyScope)); *(AudioObjectPropertyScope *)outData = kAudioObjectPropertyScopeOutput; return noErr;
        case kAudioControlPropertyElement: REQUIRE_SIZE(sizeof(AudioObjectPropertyElement)); *(AudioObjectPropertyElement *)outData = kAudioObjectPropertyElementMain; return noErr;
        case kAudioLevelControlPropertyScalarValue: REQUIRE_SIZE(sizeof(Float32)); *(Float32 *)outData = atomic_load(&gVolumeScalar); return noErr;
        case kAudioLevelControlPropertyDecibelValue: REQUIRE_SIZE(sizeof(Float32)); *(Float32 *)outData = ScalarToDB(atomic_load(&gVolumeScalar)); return noErr;
        case kAudioLevelControlPropertyDecibelRange: {
            REQUIRE_SIZE(sizeof(AudioValueRange));
            AudioValueRange r = {kMinDB, 0.0};
            *(AudioValueRange *)outData = r;
            return noErr;
        }
        case kAudioLevelControlPropertyConvertScalarToDecibels:
            REQUIRE_SIZE(sizeof(Float32));
            *(Float32 *)outData = ScalarToDB(*(Float32 *)outData);
            return noErr;
        case kAudioLevelControlPropertyConvertDecibelsToScalar:
            REQUIRE_SIZE(sizeof(Float32));
            *(Float32 *)outData = DBToScalar(*(Float32 *)outData);
            return noErr;
        case kAudioBooleanControlPropertyValue: REQUIRE_SIZE(sizeof(UInt32)); *(UInt32 *)outData = atomic_load(&gMute); return noErr;
        }
        break;
    }
    return kAudioHardwareUnknownPropertyError;
}

// Asks the host to switch rates; it stops IO and calls PerformDeviceConfigurationChange.
static OSStatus RequestRate(Float64 rate) {
    if (!IsSupportedRate(rate)) return kAudioDeviceUnsupportedFormatError;
    if (rate == CurrentRate()) return noErr;
    return gHost->RequestDeviceConfigurationChange(gHost, kObjectID_Device, (UInt64)rate, NULL);
}

static OSStatus LS50Stream_SetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *a,
                                         UInt32 qSize, const void *q, UInt32 inDataSize, const void *inData) {
    (void)pid; (void)qSize; (void)q;
    if (d != gDriverRef || !a || !inData) return kAudioHardwareIllegalOperationError;
    Boolean settable = false;
    OSStatus err = LS50Stream_IsPropertySettable(d, obj, pid, a, &settable);
    if (err) return err;
    if (!settable) return kAudioHardwareUnsupportedOperationError;

    switch (a->mSelector) {
    case kAudioStreamPropertyIsActive:
        return inDataSize == sizeof(UInt32) ? noErr : kAudioHardwareBadPropertySizeError;
    case kAudioDevicePropertyNominalSampleRate:
        if (inDataSize != sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
        return RequestRate(*(const Float64 *)inData);
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat: {
        if (inDataSize != sizeof(AudioStreamBasicDescription)) return kAudioHardwareBadPropertySizeError;
        const AudioStreamBasicDescription *f = inData;
        AudioStreamBasicDescription ours = FormatAt(f->mSampleRate);
        bool same = f->mFormatID == ours.mFormatID && f->mChannelsPerFrame == ours.mChannelsPerFrame &&
                    f->mBitsPerChannel == ours.mBitsPerChannel && f->mFormatFlags == ours.mFormatFlags;
        return same ? RequestRate(f->mSampleRate) : kAudioDeviceUnsupportedFormatError;
    }
    case kAudioLevelControlPropertyScalarValue:
    case kAudioLevelControlPropertyDecibelValue: {
        if (inDataSize != sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
        Float32 v = *(const Float32 *)inData;
        Float32 scalar = a->mSelector == kAudioLevelControlPropertyScalarValue ? v : DBToScalar(v);
        atomic_store(&gVolumeScalar, scalar < 0 ? 0 : (scalar > 1 ? 1 : scalar));
        AudioObjectPropertyAddress changed[] = {
            {kAudioLevelControlPropertyScalarValue, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
            {kAudioLevelControlPropertyDecibelValue, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        };
        gHost->PropertiesChanged(gHost, kObjectID_Volume, 2, changed);
        return noErr;
    }
    case kAudioBooleanControlPropertyValue: {
        if (inDataSize != sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
        atomic_store(&gMute, *(const UInt32 *)inData ? 1 : 0);
        AudioObjectPropertyAddress changed = {kAudioBooleanControlPropertyValue, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        gHost->PropertiesChanged(gHost, kObjectID_Mute, 1, &changed);
        return noErr;
    }
    }
    return kAudioHardwareUnknownPropertyError;
}

// ---------------------------------------------------------------------------------------------
// IO

static OSStatus LS50Stream_StartIO(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client) {
    (void)client;
    if (d != gDriverRef || id != kObjectID_Device) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gStateMutex);
    if (gIOClients++ == 0) {
        pthread_mutex_lock(&gTimeMutex);
        gNumberTimeStamps = 0;
        gAnchorHostTime = mach_absolute_time();
        pthread_mutex_unlock(&gTimeMutex);
        ++gGeneration;
        LS50StreamShmHeader *h = atomic_load(&gShm);
        if (h) PublishTimeline(h);
    }
    pthread_mutex_unlock(&gStateMutex);
    return noErr;
}

static OSStatus LS50Stream_StopIO(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client) {
    (void)client;
    if (d != gDriverRef || id != kObjectID_Device) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gStateMutex);
    if (gIOClients > 0 && --gIOClients == 0) {
        LS50StreamShmHeader *h = atomic_load(&gShm);
        if (h) __atomic_store_n(&h->io_running, 0, __ATOMIC_RELEASE);
    }
    pthread_mutex_unlock(&gStateMutex);
    return noErr;
}

static OSStatus LS50Stream_GetZeroTimeStamp(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client, Float64 *outSampleTime, UInt64 *outHostTime, UInt64 *outSeed) {
    (void)client;
    if (d != gDriverRef || id != kObjectID_Device) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gTimeMutex);
    Float64 ticksPerPeriod = gHostTicksPerFrame * kZeroTimeStampPeriod;
    UInt64 next = gAnchorHostTime + (UInt64)((Float64)(gNumberTimeStamps + 1) * ticksPerPeriod);
    if (next <= mach_absolute_time()) ++gNumberTimeStamps;
    *outSampleTime = (Float64)(gNumberTimeStamps * kZeroTimeStampPeriod);
    *outHostTime = gAnchorHostTime + (UInt64)((Float64)gNumberTimeStamps * ticksPerPeriod);
    *outSeed = 1;
    pthread_mutex_unlock(&gTimeMutex);
    return noErr;
}

static OSStatus LS50Stream_WillDoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client, UInt32 op, Boolean *outWillDo, Boolean *outInPlace) {
    (void)client;
    if (d != gDriverRef || id != kObjectID_Device) return kAudioHardwareBadObjectError;
    *outWillDo = op == kAudioServerPlugInIOOperationWriteMix;
    *outInPlace = true;
    return noErr;
}

static OSStatus LS50Stream_BeginIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *info) {
    (void)client; (void)op; (void)frames; (void)info;
    return (d == gDriverRef && id == kObjectID_Device) ? noErr : kAudioHardwareBadObjectError;
}

// Real-time IO thread: no locks, no allocation.
static OSStatus LS50Stream_DoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, AudioObjectID stream, UInt32 client, UInt32 op,
                                       UInt32 frames, const AudioServerPlugInIOCycleInfo *info, void *mainBuffer, void *secondaryBuffer) {
    (void)client; (void)secondaryBuffer;
    if (d != gDriverRef || id != kObjectID_Device || stream != kObjectID_Stream) return kAudioHardwareBadObjectError;
    if (op != kAudioServerPlugInIOOperationWriteMix || !mainBuffer) return noErr;
    LS50StreamShmHeader *h = atomic_load_explicit(&gShm, memory_order_acquire);
    if (!h) return noErr;

    float scalar = atomic_load_explicit(&gVolumeScalar, memory_order_relaxed);
    float gain = atomic_load_explicit(&gMute, memory_order_relaxed) ? 0.0f : scalar * scalar;
    const float *in = mainBuffer;
    float *ring = (float *)((char *)h + LS50STREAM_SHM_SAMPLES_OFFSET);
    Float64 sampleTime = info->mOutputTime.mSampleTime;
    UInt64 start = sampleTime > 0 ? (UInt64)sampleTime : 0;
    for (UInt32 f = 0; f < frames; f++) {
        float *dst = ring + ((start + f) % LS50STREAM_RING_FRAMES) * kChannels;
        const float *src = in + (size_t)f * kChannels;
        for (UInt32 ch = 0; ch < kChannels; ch++) dst[ch] = src[ch] * gain;
    }
    __atomic_store_n(&h->io_frames, frames, __ATOMIC_RELAXED);
    __atomic_store_n(&h->write_end, start + frames, __ATOMIC_RELEASE);
    return noErr;
}

static OSStatus LS50Stream_EndIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *info) {
    (void)client; (void)op; (void)frames; (void)info;
    return (d == gDriverRef && id == kObjectID_Device) ? noErr : kAudioHardwareBadObjectError;
}
