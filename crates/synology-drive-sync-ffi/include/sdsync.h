#ifndef SDSYNC_H
#define SDSYNC_H

#include <stdint.h>

#if defined(_WIN32)
#  if defined(SDSYNC_BUILDING_LIBRARY)
#    define SDSYNC_API __declspec(dllexport)
#  else
#    define SDSYNC_API __declspec(dllimport)
#  endif
#  define SDSYNC_CALL __cdecl
#else
#  define SDSYNC_API __attribute__((visibility("default")))
#  define SDSYNC_CALL
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define SDSYNC_ABI_VERSION_V1 UINT32_C(1)

#define SDSYNC_STATUS_OK INT32_C(0)
#define SDSYNC_STATUS_INVALID_ARGUMENT INT32_C(1)
#define SDSYNC_STATUS_CALLBACK_FAILED INT32_C(2)
#define SDSYNC_STATUS_CANCELLED INT32_C(3)
#define SDSYNC_STATUS_OPERATION_FAILED INT32_C(4)
#define SDSYNC_STATUS_PANIC INT32_C(255)

#define SDSYNC_CALLBACK_OK INT32_C(0)
#define SDSYNC_CALLBACK_UNAVAILABLE INT32_C(1)
#define SDSYNC_CALLBACK_CANCELLED INT32_C(2)

#define SDSYNC_SECRET_PASSWORD UINT32_C(1)
#define SDSYNC_SECRET_OTP_REQUIRED UINT32_C(2)
#define SDSYNC_SECRET_OTP_REJECTED UINT32_C(3)

#define SDSYNC_PLAN_PREVIEW_ONLY UINT32_C(0)
#define SDSYNC_PLAN_APPLY UINT32_C(1)
#define SDSYNC_PLAN_CANCEL UINT32_C(2)

#define SDSYNC_EVENT_CONTINUE UINT32_C(0)
#define SDSYNC_EVENT_CANCEL UINT32_C(1)

typedef struct sdsync_cancellation_v1 sdsync_cancellation_v1;
typedef struct sdsync_result_v1 sdsync_result_v1;

/*
 * Callbacks execute synchronously on the thread calling sdsync_run_v1. They
 * must not retain borrowed pointers, throw a C++ exception, unwind, or free a
 * handle that is active in that run.
 */

/*
 * Secret acquisition is a two-pass call. For the first call, buffer is NULL
 * and capacity is zero; write the required UTF-8 byte length to *written. For
 * the second call, copy exactly that many bytes and write the same length. A
 * different length or SDSYNC_CALLBACK_UNAVAILABLE on the write pass is a
 * callback protocol failure. The library immediately copies the bytes into
 * zeroizing storage and never logs them. Return SDSYNC_CALLBACK_UNAVAILABLE on
 * the query pass when the requested secret does not exist, or
 * SDSYNC_CALLBACK_CANCELLED on either pass when acquisition was cancelled.
 */
typedef int32_t (SDSYNC_CALL *sdsync_secret_callback_v1)(
    void *user_data,
    uint32_t secret_kind,
    uint8_t *buffer,
    uint64_t capacity,
    uint64_t *written);

/* json is a borrowed UTF-8 sdsync.plan.v1 document, valid only during call. */
typedef uint32_t (SDSYNC_CALL *sdsync_plan_callback_v1)(
    void *user_data,
    const uint8_t *json,
    uint64_t json_len);

/* json is a borrowed UTF-8 sdsync.event.v1 document, valid only during call. */
typedef uint32_t (SDSYNC_CALL *sdsync_event_callback_v1)(
    void *user_data,
    const uint8_t *json,
    uint64_t json_len);

typedef struct sdsync_callbacks_v1 {
    /* Set to sizeof(sdsync_callbacks_v1). */
    uint32_t struct_size;
    /* Must be zero. */
    uint32_t reserved;
    void *user_data;
    sdsync_secret_callback_v1 secret;
    sdsync_plan_callback_v1 plan;
    sdsync_event_callback_v1 event;
} sdsync_callbacks_v1;

SDSYNC_API uint32_t SDSYNC_CALL sdsync_abi_version_v1(void);

/*
 * Returns a borrowed, non-NUL-terminated UTF-8 build-version view. The pointer
 * is valid until the library is unloaded and must not be freed.
 */
SDSYNC_API int32_t SDSYNC_CALL sdsync_build_version_v1(
    const uint8_t **data,
    uint64_t *length);

SDSYNC_API int32_t SDSYNC_CALL sdsync_cancellation_new_v1(
    sdsync_cancellation_v1 **out);
SDSYNC_API int32_t SDSYNC_CALL sdsync_cancellation_cancel_v1(
    const sdsync_cancellation_v1 *cancellation);

/*
 * NULL is accepted. Otherwise free exactly once, after every concurrent run
 * using the handle has returned.
 */
SDSYNC_API void SDSYNC_CALL sdsync_cancellation_free_v1(
    sdsync_cancellation_v1 *cancellation);

/*
 * request points to a UTF-8 sdsync.request.v1 JSON document. callbacks and
 * cancellation may be NULL. With no plan callback the engine fails closed to
 * preview-only behavior. Except when out_result itself is NULL, every return
 * stores an owned result handle in *out_result, including validation failures.
 */
SDSYNC_API int32_t SDSYNC_CALL sdsync_run_v1(
    const uint8_t *request,
    uint64_t request_len,
    const sdsync_callbacks_v1 *callbacks,
    const sdsync_cancellation_v1 *cancellation,
    sdsync_result_v1 **out_result);

/*
 * Borrow the non-NUL-terminated UTF-8 sdsync.ffi-result.v1 JSON bytes. The view
 * is immutable and valid until sdsync_result_free_v1(result).
 */
SDSYNC_API int32_t SDSYNC_CALL sdsync_result_bytes_v1(
    const sdsync_result_v1 *result,
    const uint8_t **data,
    uint64_t *length);

/* NULL is accepted; a non-NULL result must be freed exactly once. */
SDSYNC_API void SDSYNC_CALL sdsync_result_free_v1(sdsync_result_v1 *result);

#ifdef __cplusplus
}
#endif

#endif /* SDSYNC_H */
