// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FDF_ENV_H_
#define LIB_FDF_ENV_H_

#include <lib/fdf/dispatcher.h>
#include <zircon/availability.h>
#include <zircon/types.h>

__BEGIN_CDECLS

// This library provides privileged operations to driver host environments for
// setting up and tearing down dispatchers
//
// Usage of this API is restricted.

typedef struct fdf_env_driver_shutdown_observer fdf_env_driver_shutdown_observer_t;

// Called when the asynchronous shutdown for all dispatchers owned by |driver| has completed.
typedef void(fdf_env_driver_shutdown_handler_t)(const void* driver,
                                                fdf_env_driver_shutdown_observer_t* observer);

// Holds context for the observer which will be called when the asynchronous shutdown
// for all dispatchers owned by a driver has completed.
//
// The client is responsible for retaining this structure in memory (and unmodified) until the
// handler runs.
struct fdf_env_driver_shutdown_observer {
  fdf_env_driver_shutdown_handler_t* handler;
};

// When new dispatchers are created, enforce that scheduler_roles specified must line up with
// roles previously registered via the `fdf_env_add_allowed_scheduler_role_for_driver` API.
#define FDF_ENV_ENFORCE_ALLOWED_SCHEDULER_ROLES ((uint32_t)1u << 0)

// Start the driver runtime. This sets up the initial thread that the dispatchers run on.
zx_status_t fdf_env_start(uint32_t options);

// Resets the driver runtime to zero threads. This may only be called when there are no
// existing dispatchers.
void fdf_env_reset();

// Same as |fdf_dispatcher_create| but allows setting the driver owner for the dispatcher.
//
// |driver| is an opaque pointer to the driver object. It will be used to uniquely identify
// the driver.
zx_status_t fdf_env_dispatcher_create_with_owner(const void* driver, uint32_t options,
                                                 const char* name, size_t name_len,
                                                 const char* scheduler_role,
                                                 size_t scheduler_role_len,
                                                 fdf_dispatcher_shutdown_observer_t* observer,
                                                 fdf_dispatcher_t** out_dispatcher);

// Dumps the state of the dispatcher to the INFO log.
void fdf_env_dispatcher_dump(fdf_dispatcher_t* dispatcher);

// DO NOT USE THIS.
// This is a temporary function added to debug https://fxbug.dev/42069837.
//
// Dumps the state of the dispatcher into |out_dump|, as a NULL terminated string.
// The caller is responsible for freeing |out_dump|.
void fdf_env_dispatcher_get_dump_deprecated(fdf_dispatcher_t* dispatcher, char** out_dump);

// Asynchronously shuts down all dispatchers owned by |driver|.
// |observer| will be notified once shutdown completes. This is guaranteed to be
// after all the dispatcher's shutdown observers have been called, and will be running
// on the thread of the final dispatcher which has been shutdown.
//
// While a driver is shutting down, no new dispatchers can be created by the driver.
//
// If this succeeds, you must keep the |observer| object alive until the
// |observer| is notified.
//
// # Errors
//
// ZX_ERR_INVALID_ARGS: No driver matching |driver| was found.
//
// ZX_ERR_BAD_STATE: A driver shutdown observer was already registered.
zx_status_t fdf_env_shutdown_dispatchers_async(const void* driver,
                                               fdf_env_driver_shutdown_observer_t* observer);

// Destroys all dispatchers in the process and blocks the current thread
// until each runtime dispatcher in the process is observed to have been destroyed.
//
// This should only be used called after all dispatchers have been shutdown.
//
// # Thread requirements
//
// This should not be called from a thread managed by the driver runtime,
// such as from tasks or ChannelRead callbacks.
void fdf_env_destroy_all_dispatchers(void);

// Notifies the runtime that we have entered a new driver context,
// such as via a Banjo call.
//
// |driver| is an opaque unique identifier for the driver.
void fdf_env_register_driver_entry(const void* driver);

// Notifies the runtime that we have exited the current driver context.
void fdf_env_register_driver_exit(void);

// Returns the driver on top of the the thread's current call stack.
// Returns NULL if no drivers are on the stack.
const void* fdf_env_get_current_driver(void);

// Returns whether the dispatcher has any queued tasks.
bool fdf_env_dispatcher_has_queued_tasks(fdf_dispatcher_t* dispatcher);

// Returns the current maximum number of threads which will be spawned for thread pool associated
// with the given scheduler role.
//
// |scheduler_role| is the name of the role which is passed when creating dispatchers.
// |scheduler_role_len | is the length of the string, without including the terminating
// NULL character.
uint32_t fdf_env_get_thread_limit(const char* scheduler_role, size_t scheduler_role_len);

// Sets the number of threads which will be spawned for thread pool associated with the given
// scheduler role. It cannot shrink the limit less to a value lower than the current number of
// threads in the thread pool.
//
// |scheduler_role| is the name of the role which is passed when creating dispatchers.
// |scheduler_role_len | is the length of the string, without including the terminating
// NULL character.
// |max_threads| is the number of threads to use as new limit.
//
// # Errors
//
// ZX_ERR_OUT_OF_RANGE: |max_threads| is less that the current number of threads.
zx_status_t fdf_env_set_thread_limit(const char* scheduler_role, size_t scheduler_role_len,
                                     uint32_t max_threads);

// Adds an allowed scheduler role for the given driver.
void fdf_env_add_allowed_scheduler_role_for_driver(const void* driver, const char* role,
                                                   size_t role_length) ZX_AVAILABLE_SINCE(27);

// Gets the opaque pointer uniquely associated with the driver currently running on the
// thread identified by |tid|.
//
// Returns the driver pointer through out parameter |out_driver|.
//
// # Errors
//
// ZX_ERR_NOT_FOUND: If the tid did not have a driver running on it, or the tid was not able
// to be identified.
//
// ZX_ERR_INVALID_ARGS: If the out_driver is not valid.
zx_status_t fdf_env_get_driver_on_tid(zx_koid_t tid, const void** out_driver)
    ZX_AVAILABLE_SINCE(27);

__END_CDECLS

#endif  // LIB_FDF_ENV_H_
