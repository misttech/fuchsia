// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/annotations/data_register.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <limits>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"

namespace forensics::feedback {
namespace {

using fuchsia_feedback::ComponentData;
using testing::Pair;
using testing::UnorderedElementsAreArray;

constexpr char kReservedNamespace[] = "reserved-namespace";

class DataRegisterTest : public UnitTestFixture {
 public:
  DataRegisterTest() { MakeNewDataRegister(); }

 protected:
  void Upsert(ComponentData data, const zx_status_t expected_status = ZX_OK) {
    zx::result endpoints = fidl::CreateEndpoints<fuchsia_feedback::ComponentDataRegister>();
    FX_CHECK(endpoints.is_ok());

    fidl::ServerBinding<fuchsia_feedback::ComponentDataRegister> binding(
        dispatcher(), std::move(endpoints->server), data_register_.get(),
        fidl::kIgnoreBindingClosure);
    fidl::Client client(std::move(endpoints->client), dispatcher());

    bool called_back = false;
    client->Upsert(std::move(data))
        .Then([&called_back, expected_status](
                  fidl::Result<fuchsia_feedback::ComponentDataRegister::Upsert>& result) {
          if (expected_status == ZX_OK) {
            FX_CHECK(result.is_ok()) << result.error_value();
          } else {
            FX_CHECK(result.is_error());
            EXPECT_TRUE(result.error_value().is_peer_closed());
            EXPECT_EQ(result.error_value().status(), expected_status);
          }
          called_back = true;
        });
    RunLoopUntilIdle();
    FX_CHECK(called_back);
  }

  std::string RegisterJsonPath() { return files::JoinPath(tmp_dir_.path(), "register.json"); }

  std::string ReadRegisterJson() {
    std::string json;
    files::ReadFileToString(RegisterJsonPath(), &json);
    return json;
  }

  void MakeNewDataRegister(const size_t max_size = std::numeric_limits<size_t>::max()) {
    data_register_ = std::make_unique<DataRegister>(
        max_size, std::set<std::string>({kReservedNamespace}), RegisterJsonPath());
  }

  files::ScopedTempDir tmp_dir_;
  std::unique_ptr<DataRegister> data_register_;
};

TEST_F(DataRegisterTest, Upsert_Basic) {
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");
}

TEST_F(DataRegisterTest, Upsert_DefaultNamespaceIfNoNamespaceProvided) {
  ComponentData data;
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("misc.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "misc": {
        "k": "v"
    }
})");
}

TEST_F(DataRegisterTest, Upsert_NoInsertionsOnEmptyAnnotations) {
  ComponentData data;

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), testing::IsEmpty());
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_TRUE(ReadRegisterJson().empty());
}

TEST_F(DataRegisterTest, Upsert_NoInsertionsOnReservedNamespace) {
  ComponentData data;
  data.namespace_(kReservedNamespace);
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data), ZX_ERR_INVALID_ARGS);

  EXPECT_THAT(data_register_->Get(), testing::IsEmpty());
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_TRUE(ReadRegisterJson().empty());
}

struct InvalidNamespaceParam {
  std::string test_name;
  std::string namespace_;
};

class InvalidNamespaceDataRegisterTest : public DataRegisterTest,
                                         public testing::WithParamInterface<InvalidNamespaceParam> {
};

INSTANTIATE_TEST_SUITE_P(WithVariousInvalidNamespaces, InvalidNamespaceDataRegisterTest,
                         testing::ValuesIn(std::vector<InvalidNamespaceParam>({
                             {
                                 "Empty",
                                 "",
                             },
                             {
                                 "ContainsPeriod",
                                 "namespace.with.period",
                             },
                             {
                                 "ReservedPrefixWithPeriod",
                                 std::string(kReservedNamespace) + ".sub",
                             },
                             {
                                 "ContainsUnderscore",
                                 "namespace_with_underscore",
                             },
                             {
                                 "ContainsUppercase",
                                 "NamespaceWithUppercase",
                             },
                             {
                                 "ContainsDigit",
                                 "namespace1",
                             },
                         })),
                         [](const testing::TestParamInfo<InvalidNamespaceParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(InvalidNamespaceDataRegisterTest, Upsert_NoInsertionsOnInvalidNamespace) {
  ComponentData data;
  data.namespace_(GetParam().namespace_);
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data), ZX_ERR_INVALID_ARGS);

  EXPECT_THAT(data_register_->Get(), testing::IsEmpty());
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_TRUE(ReadRegisterJson().empty());
}

struct InvalidKeyParam {
  std::string test_name;
  std::string key;
};

class InvalidKeyDataRegisterTest : public DataRegisterTest,
                                   public testing::WithParamInterface<InvalidKeyParam> {};

INSTANTIATE_TEST_SUITE_P(WithVariousInvalidKeys, InvalidKeyDataRegisterTest,
                         testing::ValuesIn(std::vector<InvalidKeyParam>({
                             {
                                 "Empty",
                                 "",
                             },
                             {
                                 "ContainsUnderscore",
                                 "key_with_underscore",
                             },
                             {
                                 "ContainsUppercase",
                                 "KeyWithUppercase",
                             },
                             {
                                 "ContainsDigit",
                                 "key1",
                             },
                         })),
                         [](const testing::TestParamInfo<InvalidKeyParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(InvalidKeyDataRegisterTest, Upsert_NoInsertionsOnInvalidKey) {
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"valid-key.with.period", "v"},
      {GetParam().key, "v"},
  }});

  Upsert(std::move(data), ZX_ERR_INVALID_ARGS);

  EXPECT_THAT(data_register_->Get(), testing::IsEmpty());
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_TRUE(ReadRegisterJson().empty());
}

TEST_F(DataRegisterTest, Upsert_NoInsertionsOnTooMany) {
  MakeNewDataRegister(/*max_size=*/1);
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");

  ComponentData data2;
  data2.namespace_("namespace");
  data2.annotations({{
      {"k-two", "v2"},
  }});

  Upsert(std::move(data2));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_TRUE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");
}

TEST_F(DataRegisterTest, Upsert_NoUpdatesOnEmptyAnnotations) {
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");

  // We upsert another ComponentData with no annotations.
  ComponentData data2;

  Upsert(std::move(data2));

  // We check that the DataRegister's namespaced annotations and AttachmentManager's non-platform
  // annotations are still the same.
  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");
}

TEST_F(DataRegisterTest, Upsert_InsertIfDifferentNamespaces) {
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");

  // We upsert another ComponentData with the same annotations, but under a different namespace.
  ComponentData data2;
  data2.namespace_("namespace-two");
  data2.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data2));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                         Pair("namespace-two.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    },
    "namespace-two": {
        "k": "v"
    }
})");
}

TEST_F(DataRegisterTest, Upsert_InsertIfDifferentKey) {
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());

  // We upsert another ComponentData under the same namespace, but with a different key.
  ComponentData data2;
  data2.namespace_("namespace");
  data2.annotations({{
      {"k.two", "v2"},
  }});

  Upsert(std::move(data2));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                         Pair("namespace.k.two", ErrorOrString("v2")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v",
        "k.two": "v2"
    }
})");
}

TEST_F(DataRegisterTest, Upsert_UpdateIfSameKey) {
  ComponentData data;
  data.namespace_("namespace");
  data.annotations({{
      {"k", "v"},
  }});

  Upsert(std::move(data));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v"
    }
})");

  // We upsert another ComponentData under the same namespace and the same key.
  ComponentData data2;
  data2.namespace_("namespace");
  data2.annotations({{
      {"k", "v2"},
  }});

  Upsert(std::move(data2));

  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace.k", ErrorOrString("v2")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
  EXPECT_EQ(ReadRegisterJson(), R"({
    "namespace": {
        "k": "v2"
    }
})");
}

TEST_F(DataRegisterTest, ReinitializesFromJson) {
  ComponentData data1;
  data1.namespace_("namespace-one");
  data1.annotations({{
      {"k-one", "v1"},
      {"k-two", "v2"},
  }});

  Upsert(std::move(data1));

  ComponentData data2;
  data2.namespace_("namespace-two");
  data2.annotations({{
      {"k-three", "v3"},
      {"k-four", "v4"},
  }});

  Upsert(std::move(data2));

  MakeNewDataRegister();
  EXPECT_THAT(data_register_->Get(), UnorderedElementsAreArray({
                                         Pair("namespace-one.k-one", ErrorOrString("v1")),
                                         Pair("namespace-one.k-two", ErrorOrString("v2")),
                                         Pair("namespace-two.k-three", ErrorOrString("v3")),
                                         Pair("namespace-two.k-four", ErrorOrString("v4")),
                                     }));
  EXPECT_FALSE(data_register_->IsMissingAnnotations());
}

}  // namespace
}  // namespace forensics::feedback
