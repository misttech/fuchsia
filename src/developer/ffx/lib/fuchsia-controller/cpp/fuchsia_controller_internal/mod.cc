// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "mod.h"

#include <fidl/fuchsia.fdomain/cpp/fidl.h>
#include <fidl/fuchsia.fdomain/cpp/natural_ostream.h>
#include <lib/stdcompat/span.h>

#include <sstream>

#include "error.h"

// Defined in fuchsia_controller_py.cc
extern struct PyModuleDef fuchsia_controller_internal;

namespace {
// Takes a PyTuple and sets the decode error (if we encountered one).
template <typename T, typename V>
PyObject *get_decode_error(const fit::result<T, V> &decode_res) {
  std::ostringstream ss;
  ss << "Unable to decode underlying FIDL error from buffer (this is a bug). Code: "
     << decode_res.error_value();
  auto str = ss.str();
  return PyUnicode_FromStringAndSize(str.data(), static_cast<Py_ssize_t>(str.size()));
}

// Decodes a FIDL error from the scratch memory and turns it into a Python exception.
template <typename T>
PyObject *decode_wire_error_type(mod::FuchsiaControllerState *state) {
  uint64_t fidl_msg_len = *reinterpret_cast<uint64_t *>(state->ERR_SCRATCH);
  if (fidl_msg_len > mod::ERR_SCRATCH_LEN) {
    std::ostringstream ss;
    ss << "Attempted to parse FIDL object of size " << fidl_msg_len
       << " which is beyond the max size of " << mod::ERR_SCRATCH_LEN
       << ". This is likely a malformed error. ";
    auto str = ss.str();
    PyErr_SetString(PyExc_RuntimeError, str.c_str());
    return nullptr;
  }
  fit::result decode_res = fidl::Unpersist<T>(cpp20::span(
      reinterpret_cast<uint8_t *>(state->ERR_SCRATCH + sizeof(fidl_msg_len)), fidl_msg_len));
  if (decode_res.is_error()) {
    return get_decode_error(decode_res);
  }
  std::ostringstream ss;
  // For the time being there's not any existing code that handles the various
  // kinds of errors that this could turn into. We're going to turn it into a
  // somewhat readable string, and depending on use-cases we can add
  // easier-to-debug information later. Ideally in the future we can leverage
  // Python bindings and simply use some kind of `unpersist` function for that
  // instead.
  //
  // There is a bit of a small drop in readability for integer-based errors.
  // For example, PEER_CLOSED, which is still possible in target errors, would
  // be rendered as -24
  auto fostream = fidl::ostream::Formatted(decode_res.value());
  ss << fostream;
  auto output = ss.str();
  return PyUnicode_FromStringAndSize(output.data(), static_cast<Py_ssize_t>(output.size()));
}

void set_fdomain_exception(mod::FuchsiaControllerState *state, fc_status_t err) {
  PyObject *tuple = PyTuple_New(2);
  if (tuple == nullptr) {
    std::ostringstream ss;
    ss << "Failed to allocate Tuple in %s" << __func__;
    auto out = ss.str();
    PyErr_SetString(PyExc_RuntimeError, out.c_str());
    return;
  }
  PyTuple_SetItem(tuple, 0, PyLong_FromLong(err));
  PyObject *err_message = nullptr;
  switch (err) {
    case FC_ERR_SOCKET_WRITE:
      err_message = decode_wire_error_type<::fuchsia_fdomain::WriteSocketError>(state);
      break;
    case FC_ERR_CHANNEL_WRITE: {
      err_message = decode_wire_error_type<::fuchsia_fdomain::WriteChannelError>(state);
      break;
    }
    case FC_ERR_FDOMAIN: {
      err_message = decode_wire_error_type<::fuchsia_fdomain::Error>(state);
      break;
    }
    default:
      std::ostringstream ss;
      // It's a little awkward, but in the event that the caller just sent something wrong we should
      // let the user know they've run into a bug. This would only happen if "set_python_exception"
      // was written incorrectly.
      ss << "Received unrecognized fc_status_t error (" << err << ") in " << __func__
         << ". This is a bug";
      auto out = ss.str();
      PyErr_SetString(PyExc_RuntimeError, out.c_str());
      break;
  }
  if (err_message != nullptr) {
    PyTuple_SetItem(tuple, 1, err_message);
    PyErr_SetObject(reinterpret_cast<PyObject *>(error::FcTransportStatusType), tuple);
  }
  // Tuple ref is in PyErr_SetObject at this point. And if it isn't, we don't need it anymore.
  Py_XDECREF(tuple);
}
}  // namespace

namespace mod {

FuchsiaControllerState *get_module_state() {
  auto mod = PyState_FindModule(&fuchsia_controller_internal);
  if (mod == nullptr) {
    return nullptr;
  }
  return reinterpret_cast<FuchsiaControllerState *>(PyModule_GetState(mod));
}

void set_python_exception(fc_status_t err) {
  auto state = get_module_state();
  switch (err) {
    // If for some reason this is passed, some bug has been introduced, as there
    // is no reason to return `FC_OK`.
    case FC_OK:
      PyErr_SetString(PyExc_RuntimeError,
                      "FC_OK was passed to raise an exception. This is likely a bug");
      break;
    // None of these cases contain any internal structs, so setting the error
    // code is sufficient.
    case FC_ERR_INVALID_ARGS:
    case FC_ERR_NOT_SUPPORTED:
    case FC_ERR_NOT_FOUND:
    case FC_ERR_BUFFER_TOO_SMALL:
    case FC_ERR_SHOULD_WAIT:
    case FC_ERR_PROTOCOL_OBJECT_TYPE_INCOMPATIBLE:
    case FC_ERR_PROTOCOL_RIGHTS_INCOMPATIBLE:
    case FC_ERR_PROTOCOL_STREAM_EVENT_INCOMPATIBLE:
    case FC_ERR_CONNECTION_MISMATCH:
    case FC_ERR_STREAMING_ABORTED: {
      auto exception_err = PyLong_FromLong(err);
      PyErr_SetObject(reinterpret_cast<PyObject *>(error::FcTransportStatusType), exception_err);
      Py_XDECREF(exception_err);
      break;
    }
    // These errors are only surfaced from internal failures, which means the
    // Rust error that created this exception is surfaced.
    case FC_ERR_PROTOCOL:
    case FC_ERR_INTERNAL:
    case FC_ERR_TRANSPORT: {
      uint64_t error_len = *reinterpret_cast<uint64_t *>(state->ERR_SCRATCH);
      auto str_obj = PyUnicode_FromStringAndSize(state->ERR_SCRATCH + sizeof(uint64_t),
                                                 static_cast<Py_ssize_t>(error_len));
      // If this is null this will return an internal error about allocating the string.
      if (str_obj == nullptr) {
        PyErr_Clear();
        PyErr_SetString(
            PyExc_RuntimeError,
            "Internal error: unable to allocate error string from buffer. This is a bug");
        return;
      }
      PyErr_SetObject(PyExc_RuntimeError, str_obj);
      Py_XDECREF(str_obj);
      break;
    }
    case FC_ERR_SOCKET_WRITE:
    case FC_ERR_CHANNEL_WRITE:
    case FC_ERR_FDOMAIN: {
      ::set_fdomain_exception(state, err);
      break;
    }
    default: {
      std::ostringstream ss;
      ss << "Received unrecognized fc_status_t (" << err << ")";
      auto out = ss.str();
      PyErr_SetString(PyExc_RuntimeError, out.c_str());
      break;
    }
  }
}

}  // namespace mod
