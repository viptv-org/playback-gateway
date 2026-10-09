// SPDX-License-Identifier: GPL-2.0-only
#include <jni.h>
#include <stdlib.h>
#include <string.h>
#include "libtorrent_runtime.h"

JNIEXPORT jbyteArray JNICALL
Java_org_playbackgateway_runtime_NativeRuntime_callNative(
    JNIEnv *env, jclass type, jbyteArray request) {
    (void) type;
    if (request == NULL) return NULL;
    jsize length = (*env)->GetArrayLength(env, request);
    if (length > 6 * 1024 * 1024) return NULL;
    jbyte *input = (*env)->GetByteArrayElements(env, request, NULL);
    if (input == NULL) return NULL;
    char *response = VTRCall((char *) input, (int) length);
    (*env)->ReleaseByteArrayElements(env, request, input, JNI_ABORT);
    if (response == NULL) return NULL;
    jsize count = (jsize) strlen(response);
    jbyteArray result = (*env)->NewByteArray(env, count);
    if (result != NULL) {
        (*env)->SetByteArrayRegion(env, result, 0, count, (const jbyte *) response);
    }
    VTRFree(response);
    return result;
}
