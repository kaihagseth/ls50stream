// Reads and sets the Mac's default output device by UID, so ls50streamd can hand
// audio back to the output that was used before ours.
#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>

static AudioObjectPropertyAddress global(AudioObjectPropertySelector selector) {
    AudioObjectPropertyAddress address = {selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    return address;
}

// Writes the default output device's UID to `buf`; 0 on success.
int ls50stream_default_output_uid(char *buf, int len) {
    AudioObjectPropertyAddress address = global(kAudioHardwarePropertyDefaultOutputDevice);
    AudioObjectID device = kAudioObjectUnknown;
    UInt32 size = sizeof device;
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &device) != noErr ||
        device == kAudioObjectUnknown)
        return -1;
    address = global(kAudioDevicePropertyDeviceUID);
    CFStringRef uid = NULL;
    size = sizeof uid;
    if (AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &uid) != noErr || !uid) return -1;
    Boolean ok = CFStringGetCString(uid, buf, len, kCFStringEncodingUTF8);
    CFRelease(uid);
    return ok ? 0 : -1;
}

// Makes the device with this UID the default output; 0 on success.
int ls50stream_set_default_output_uid(const char *uid_cstr) {
    CFStringRef uid = CFStringCreateWithCString(NULL, uid_cstr, kCFStringEncodingUTF8);
    if (!uid) return -1;
    AudioObjectID device = kAudioObjectUnknown;
    AudioValueTranslation translation = {&uid, sizeof uid, &device, sizeof device};
    AudioObjectPropertyAddress address = global(kAudioHardwarePropertyDeviceForUID);
    UInt32 size = sizeof translation;
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &translation);
    CFRelease(uid);
    if (status != noErr || device == kAudioObjectUnknown) return -1;
    address = global(kAudioHardwarePropertyDefaultOutputDevice);
    return AudioObjectSetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, sizeof device, &device) == noErr ? 0 : -1;
}
