// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
/dts-v1/;

/ {
	test-board {
		test-soc {
			pmic: test-pmic {
				compatible = "some-pmic";

				default_tcpc: tcpc {
				};

				port0_tcpc: tcpc-port0 {
					usb-connector-output-name = "typec0";
				};

				port1_tcpc: tcpc-port1 {
					usb-connector-output-name = "typec1";
				};
			};

			default_connector_device: default-connector-device {
				compatible = "test-usb-phy";
				usb-connector = <&default_tcpc>;
			};

			named_connector_device: named-connector-device {
				compatible = "test-usb-phy";
				usb-connector = <&port0_tcpc>;
				usb-connector-names = "custom-connector";
			};

			multi_connector_device: multi-connector-device {
				compatible = "test-usb-phy";
				usb-connector = <&port0_tcpc>, <&port1_tcpc>;
				usb-connector-names = "port0", "port1";
			};
		};
	};
};
