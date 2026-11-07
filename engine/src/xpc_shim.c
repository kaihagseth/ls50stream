// XPC listener that hands the shared audio region to LS50Stream.driver.
// C because XPC's API is block-based; called once from Rust (src/shm.rs).

#include <xpc/xpc.h>
#include <dispatch/dispatch.h>
#include <sys/types.h>

static xpc_object_t g_shmem;
static uid_t g_allowed_uid;

static int peer_allowed(xpc_connection_t peer) {
    uid_t uid = xpc_connection_get_euid(peer);
    return uid == g_allowed_uid || uid == 0;
}

// Returns 0 on success. `region` must be mmap'ed MAP_SHARED and stay mapped for the process lifetime.
int ls50stream_xpc_listen(const char *service, void *region, size_t size, uid_t allowed_uid) {
    g_shmem = xpc_shmem_create(region, size);
    if (!g_shmem) {
        return -1;
    }
    g_allowed_uid = allowed_uid;
    dispatch_queue_t queue = dispatch_queue_create("io.github.kaihagseth.ls50stream.xpc", DISPATCH_QUEUE_SERIAL);
    xpc_connection_t listener =
        xpc_connection_create_mach_service(service, queue, XPC_CONNECTION_MACH_SERVICE_LISTENER);
    if (!listener) {
        return -2;
    }
    xpc_connection_set_event_handler(listener, ^(xpc_object_t peer) {
        if (xpc_get_type(peer) != XPC_TYPE_CONNECTION) {
            return;
        }
        if (!peer_allowed(peer)) {
            xpc_connection_cancel(peer);
            return;
        }
        xpc_connection_set_event_handler(peer, ^(xpc_object_t msg) {
            if (xpc_get_type(msg) != XPC_TYPE_DICTIONARY) {
                return;
            }
            xpc_object_t reply = xpc_dictionary_create_reply(msg);
            if (!reply) {
                return;
            }
            xpc_dictionary_set_value(reply, "shm", g_shmem);
            xpc_connection_send_message(peer, reply);
            xpc_release(reply);
        });
        xpc_connection_resume(peer);
    });
    xpc_connection_resume(listener);
    return 0;
}
