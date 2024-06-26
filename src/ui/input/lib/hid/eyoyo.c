// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/hid/acer12.h>
#include <string.h>
#include <zircon/errors.h>

// Eyoyo 10 Inch IPS EDP 1920 x 1200 Touch Screen
static const uint8_t eyoyo_touch_report_desc[] = {
    0x05, 0x0D,        // Usage Page (Digitizer)
    0x09, 0x04,        // Usage (Touch Screen)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x01,        //   Report ID (1)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x42,        //     Usage (Tip Switch)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x09, 0x51,        //     Usage (0x51)
    0x25, 0x1F,        //     Logical Maximum (31)
    0x75, 0x05,        //     Report Size (5)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x65, 0x11,        //     Unit (System: SI Linear, Length: Centimeter)
    0x09, 0x30,        //     Usage (X)
    0x35, 0x00,        //     Physical Minimum (0)
    0x46, 0xD3, 0x0A,  //     Physical Maximum (2771)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x46, 0x19, 0x06,  //     Physical Maximum (1561)
    0x09, 0x31,        //     Usage (Y)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x45, 0x00,        //     Physical Maximum (0)
    0xC0,              //   End Collection
    0x05, 0x0D,        //   Usage Page (Digitizer)
    0x15, 0x00,        //   Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00,  //   Logical Maximum (65534)
    0x75, 0x10,                    //   Report Size (16)
    0x95, 0x01,                    //   Report Count (1)
    0x09, 0x56,                    //   Usage (0x56)
    0x81, 0x02,        //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x25, 0x0A,        //   Logical Maximum (10)
    0x75, 0x08,        //   Report Size (8)
    0x09, 0x54,        //   Usage (0x54)
    0x81, 0x02,        //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x85, 0x42,        //   Report ID (66)
    0x09, 0x55,        //   Usage (0x55)
    0x25, 0x0A,        //   Logical Maximum (10)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x85, 0x44,        //   Report ID (68)
    0x06, 0x00, 0xFF,  //   Usage Page (Vendor Defined 0xFF00)
    0x09, 0xC5,        //   Usage (0xC5)
    0x26, 0xFF, 0x00,  //   Logical Maximum (255)
    0x96, 0x00, 0x01,  //   Report Count (256)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x85, 0xF2,        //   Report ID (-14)
    0x09, 0x03,        //   Usage (0x03)
    0x75, 0x08,        //   Report Size (8)
    0x95, 0x04,        //   Report Count (4)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x09, 0x04,        //   Usage (0x04)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x09, 0x05,        //   Usage (0x05)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x95, 0x01,        //   Report Count (1)
    0x09, 0x06,        //   Usage (0x06)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x09, 0x07,        //   Usage (0x07)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x85, 0x06,        //   Report ID (6)
    0x09, 0x01,        //   Usage (0x01)
    0x95, 0x07,        //   Report Count (7)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x85, 0x08,        //   Report ID (8)
    0x09, 0x01,        //   Usage (0x01)
    0x95, 0x3F,        //   Report Count (63)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x85, 0x07,        //   Report ID (7)
    0x09, 0x01,        //   Usage (0x01)
    0x95, 0x3F,        //   Report Count (63)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0x85, 0xF4,        //   Report ID (-12)
    0x09, 0x01,        //   Usage (0x01)
    0x95, 0x2F,        //   Report Count (47)
    0xB1, 0x02,        //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //   Position,Non-volatile)
    0xC0,              // End Collection
    0x05, 0x0D,        // Usage Page (Digitizer)
    0x09, 0x0E,        // Usage (0x0E)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x43,        //   Report ID (67)
    0x09, 0x23,        //   Usage (0x23)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x52,        //     Usage (0x52)
    0x09, 0x53,        //     Usage (0x53)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x0A,        //     Logical Maximum (10)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x02,        //     Report Count (2)
    0xB1, 0x02,        //     Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null
                       //     Position,Non-volatile)
    0xC0,              //   End Collection
    0xC0,              // End Collection
    0x05, 0x01,        // Usage Page (Generic Desktop Ctrls)
    0x09, 0x02,        // Usage (Mouse)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x03,        //   Report ID (3)
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x05, 0x09,        //     Usage Page (Button)
    0x19, 0x01,        //     Usage Minimum (0x01)
    0x29, 0x02,        //     Usage Maximum (0x02)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x06,        //     Report Count (6)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x09, 0x30,        //     Usage (X)
    0x09, 0x31,        //     Usage (Y)
    0x75, 0x10,        //     Report Size (16)
    0x95, 0x02,        //     Report Count (2)
    0x15, 0x00,        //     Logical Minimum (0)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0xC0,              //   End Collection
    0xC0,              // End Collection

    // 814 bytes
};

bool is_eyoyo_touch_report_desc(const uint8_t* data, size_t len) {
  if (!data)
    return false;

  if (len != sizeof(eyoyo_touch_report_desc))
    return false;

  return (memcmp(data, eyoyo_touch_report_desc, len) == 0);
}
