#include "sdsync.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int32_t SDSYNC_CALL provide_secret(
    void *user_data,
    uint32_t secret_kind,
    uint8_t *buffer,
    uint64_t capacity,
    uint64_t *written) {
    (void)user_data;
    const char *name = secret_kind == SDSYNC_SECRET_PASSWORD
        ? "SDSYNC_PASSWORD"
        : "SDSYNC_OTP";
    const char *value = getenv(name);
    if (value == NULL) {
        return SDSYNC_CALLBACK_UNAVAILABLE;
    }
    const size_t length = strlen(value);
    if (buffer == NULL) {
        *written = (uint64_t)length;
        return SDSYNC_CALLBACK_OK;
    }
    if (capacity < (uint64_t)length) {
        return SDSYNC_CALLBACK_UNAVAILABLE;
    }
    memcpy(buffer, value, length);
    *written = (uint64_t)length;
    return SDSYNC_CALLBACK_OK;
}

static uint32_t SDSYNC_CALL inspect_plan(
    void *user_data,
    const uint8_t *json,
    uint64_t json_len) {
    (void)user_data;
    (void)fwrite(json, 1, (size_t)json_len, stderr);
    (void)fputc('\n', stderr);
    const char *apply = getenv("SDSYNC_APPLY");
    return apply != NULL && strcmp(apply, "1") == 0
        ? SDSYNC_PLAN_APPLY
        : SDSYNC_PLAN_PREVIEW_ONLY;
}

static uint32_t SDSYNC_CALL print_event(
    void *user_data,
    const uint8_t *json,
    uint64_t json_len) {
    (void)user_data;
    (void)fwrite(json, 1, (size_t)json_len, stderr);
    (void)fputc('\n', stderr);
    return SDSYNC_EVENT_CONTINUE;
}

static uint8_t *read_file(const char *path, uint64_t *length) {
    FILE *file = fopen(path, "rb");
    if (file == NULL || fseek(file, 0, SEEK_END) != 0) {
        if (file != NULL) fclose(file);
        return NULL;
    }
    const long size = ftell(file);
    if (size <= 0 || fseek(file, 0, SEEK_SET) != 0) {
        fclose(file);
        return NULL;
    }
    uint8_t *bytes = (uint8_t *)malloc((size_t)size);
    if (bytes == NULL || fread(bytes, 1, (size_t)size, file) != (size_t)size) {
        free(bytes);
        fclose(file);
        return NULL;
    }
    fclose(file);
    *length = (uint64_t)size;
    return bytes;
}

int main(int argc, char **argv) {
    if (sdsync_abi_version_v1() != SDSYNC_ABI_VERSION_V1) {
        fputs("incompatible sdsync C ABI version\n", stderr);
        return 2;
    }
    if (argc != 2) {
        fprintf(stderr, "usage: %s REQUEST.json\n", argv[0]);
        return 2;
    }
    uint64_t request_len = 0;
    uint8_t *request = read_file(argv[1], &request_len);
    if (request == NULL) {
        fputs("failed to read request JSON\n", stderr);
        return 2;
    }

    sdsync_callbacks_v1 callbacks = {
        (uint32_t)sizeof(sdsync_callbacks_v1),
        0,
        NULL,
        provide_secret,
        inspect_plan,
        print_event
    };
    sdsync_result_v1 *result = NULL;
    const int32_t status = sdsync_run_v1(
        request,
        request_len,
        &callbacks,
        NULL,
        &result);
    free(request);

    if (result != NULL) {
        const uint8_t *json = NULL;
        uint64_t json_len = 0;
        if (sdsync_result_bytes_v1(result, &json, &json_len) == SDSYNC_STATUS_OK) {
            (void)fwrite(json, 1, (size_t)json_len, stdout);
            (void)fputc('\n', stdout);
        }
        sdsync_result_free_v1(result);
    }
    return status == SDSYNC_STATUS_OK ? 0 : 1;
}
