// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_TESTING_GMATCHERS_H_
#define SRC_DEVELOPER_FORENSICS_TESTING_GMATCHERS_H_

#include <sstream>
#include <string>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/attachments/types.h"
#include "src/developer/forensics/utils/errors.h"

namespace forensics {
namespace internal {

// Allows expectations to be set on AttachmentValue. Needed because values can't easily be compared
// for equality due to the move-only nature of AttachmentValue.
template <typename T>
class AttachmentDataOrValueMatcher : public ::testing::MatcherInterface<const T&> {
 public:
  using is_gtest_matcher = void;

  AttachmentDataOrValueMatcher(std::string value)
      : value_(std::move(value)), error_(std::nullopt) {}
  AttachmentDataOrValueMatcher(const Error error) : value_(std::nullopt), error_(error) {}
  AttachmentDataOrValueMatcher(std::string value, const Error error)
      : value_(std::move(value)), error_(error) {}

  bool MatchAndExplain(const T& val, ::testing::MatchResultListener* listener) const override {
    const bool value_matches =
        val.HasValue() == value_.has_value() && (!val.HasValue() || val.Value() == *value_);
    const bool error_matches =
        val.HasError() == error_.has_value() && (!val.HasError() || val.Error() == *error_);

    if (value_matches && error_matches) {
      return true;
    }

    *listener << "expected attachment value " << Describe();
    return false;
  }

  void DescribeTo(::std::ostream* os) const override { *os << Describe(); }

  void DescribeNegationTo(::std::ostream* os) const override { *os << " not " << Describe(); }

 private:
  std::string Describe() const {
    std::stringstream ss;
    if (value_.has_value()) {
      ss << "has a value \"" << *value_ << "\"";
    } else {
      ss << "does not have a value";
    }

    ss << " and ";

    if (error_.has_value()) {
      ss << "has an error \"" << ToString(*error_) << "\"";
    } else {
      ss << "does not have an error";
    }

    return ss.str();
  }

  std::optional<std::string> value_;
  std::optional<Error> error_;
};

}  // namespace internal

// Returns true if gMock |arg|.key matches |expected_key|.
MATCHER_P(MatchesKey, expected_key,
          "matches an element with key '" + std::string(expected_key) + "'") {
  return arg.key == expected_key;
}

MATCHER(HasValue, negation ? "is error" : "has value") {
  if (arg.HasValue()) {
    return true;
  }

  return false;
}

inline testing::Matcher<feedback::AttachmentValue> AttachmentValueIs(std::string data) {
  return ::testing::Matcher<feedback::AttachmentValue>(
      new internal::AttachmentDataOrValueMatcher<feedback::AttachmentValue>(std::move(data)));
}

inline testing::Matcher<feedback::AttachmentValue> AttachmentValueIs(const Error error) {
  return ::testing::Matcher<feedback::AttachmentValue>(
      new internal::AttachmentDataOrValueMatcher<feedback::AttachmentValue>(error));
}

inline testing::Matcher<feedback::AttachmentValue> AttachmentValueIs(std::string data,
                                                                     const Error error) {
  return ::testing::Matcher<feedback::AttachmentValue>(
      new internal::AttachmentDataOrValueMatcher<feedback::AttachmentValue>(std::move(data),
                                                                            error));
}

inline testing::Matcher<feedback::AttachmentData> AttachmentDataIs(std::string data) {
  return ::testing::Matcher<feedback::AttachmentData>(
      new internal::AttachmentDataOrValueMatcher<feedback::AttachmentData>(std::move(data)));
}

inline testing::Matcher<feedback::AttachmentData> AttachmentDataIs(const Error error) {
  return ::testing::Matcher<feedback::AttachmentData>(
      new internal::AttachmentDataOrValueMatcher<feedback::AttachmentData>(error));
}

inline testing::Matcher<feedback::AttachmentData> AttachmentDataIs(std::string data,
                                                                   const Error error) {
  return ::testing::Matcher<feedback::AttachmentData>(
      new internal::AttachmentDataOrValueMatcher<feedback::AttachmentData>(std::move(data), error));
}

}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_TESTING_GMATCHERS_H_
