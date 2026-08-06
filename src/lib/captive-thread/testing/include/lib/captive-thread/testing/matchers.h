// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_CAPTIVE_THREAD_TESTING_INCLUDE_LIB_CAPTIVE_THREAD_TESTING_MATCHERS_H_
#define SRC_LIB_CAPTIVE_THREAD_TESTING_INCLUDE_LIB_CAPTIVE_THREAD_TESTING_MATCHERS_H_

#include <lib/captive-thread/captive-thread.h>
#include <zircon/exception.h>

#include <ios>

#include <gmock/gmock.h>

namespace captive_thread::testing {

using namespace std::string_literals;

// This is a helper used by some matchers below to match on values of type
// zx::result<CaptiveThread*> as returned by CaptiveThread::WaitForException().
inline CaptiveThread* MatcherThread(zx::result<CaptiveThread*> result, auto* result_listener) {
  if (result.is_ok()) {
    return *result;
  }
  *result_listener << "where thread not ready: " << result.status_string();
  return nullptr;
}

// This matches a CaptiveThread::WaitForException() return value when the
// thread has stopped in an exception.
MATCHER(GotException, "") {
  const CaptiveThread* thread = MatcherThread(arg, result_listener);
  if (thread && !thread->InException()) {
    *result_listener << ::testing::PrintToString(*thread);
  }
  return thread && thread->InException();
}

// With another matcher as argument, it matches an exception with whose
// zx_exception_report_t is matched by that other matcher.
MATCHER_P(GotException, matcher,
          (negation ? "not "s : ""s) + "in exception that "s +
              ::testing::DescribeMatcher<zx_exception_report_t>(matcher, negation)) {
  const CaptiveThread* thread = MatcherThread(arg, result_listener);
  if (thread && !thread->InException()) {
    *result_listener << ::testing::PrintToString(*thread);
  }
  return thread && thread->InException() &&
         ::testing::ExplainMatchResult(matcher, *thread->ExceptionReport(), result_listener);
}

// This is a helper used by some matchers below to match on values either of
// type zx_exception_report_t when the matcher is passed into GotException(),
// or of the type GotException() itself matches on, so as to implicitly wrap
// GotException() around that matcher.
constexpr const zx_exception_report_t* MatcherExceptionReport(  //
    const zx_exception_report_t& report, auto* result_listener) {
  return &report;
}

inline std::optional<zx_exception_report_t> MatcherExceptionReport(  //
    zx::result<CaptiveThread*> result, auto* result_listener) {
  std::optional<zx_exception_report_t> report;
  if (result.is_error()) {
    *result_listener << "where thread not ready: " << result.status_string();
  } else {
    report = result->ExceptionReport();
    if (!report) {
      *result_listener << "where thread is not in any exception";
    }
  }
  return report;
}

// This is a matcher on zx_exception_report_t that wraps a matcher on the
// zx_excp_type_t value.
MATCHER_P(WithType, matcher,
          (negation ? "not type "s : "type "s) +
              ::testing::DescribeMatcher<zx_excp_type_t>(matcher, negation)) {
  auto report = MatcherExceptionReport(arg, result_listener);
  if (!report) {
    return false;
  }
  const zx_excp_type_t excp_type = report->header.type;
  *result_listener << "where the type is " << zx_exception_get_string(excp_type) << " ("
                   << std::showbase << std::hex << excp_type << ")";
  return ::testing::ExplainMatchResult(matcher, excp_type, result_listener);
}

// This is a composite matcher used inside GotException() to match the
// canonical exception type induced by __builtin_trap() (and many other ways).
MATCHER(IsTrap, "") {
  return ::testing::ExplainMatchResult(WithType(kTrapException), arg, result_listener);
}

// This is a composite matcher used inside GotException() to match any page
// fault.
MATCHER(IsPageFault, "") {
  return ::testing::ExplainMatchResult(WithType(ZX_EXCP_FATAL_PAGE_FAULT), arg, result_listener);
}

// This is a composite matcher used inside GotException() to match a page fault
// whose fault address is matched by the argument matcher.
MATCHER_P(IsPageFault, matcher,
          (negation ? "not "s : ""s) + "page fault at address that "s +
              ::testing::DescribeMatcher<uint64_t>(matcher, negation)) {
  using ::testing::AllOf;
  using ::testing::ResultOf;
  auto report = MatcherExceptionReport(arg, result_listener);
  if (!report) {
    return false;
  }
  auto address_matcher = ResultOf("fault address", FaultAddress, matcher);
  return ::testing::ExplainMatchResult(  //
      AllOf(IsPageFault(), address_matcher), *report, result_listener);
}

}  // namespace captive_thread::testing

#endif  // SRC_LIB_CAPTIVE_THREAD_TESTING_INCLUDE_LIB_CAPTIVE_THREAD_TESTING_MATCHERS_H_
