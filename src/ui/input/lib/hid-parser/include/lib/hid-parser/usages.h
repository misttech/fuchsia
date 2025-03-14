// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_INPUT_LIB_HID_PARSER_INCLUDE_LIB_HID_PARSER_USAGES_H_
#define SRC_UI_INPUT_LIB_HID_PARSER_INCLUDE_LIB_HID_PARSER_USAGES_H_

#include <stdint.h>

namespace hid {
namespace usage {

constexpr uint16_t UsageToUsagePage(uint32_t usage) { return static_cast<uint16_t>(usage >> 16); }

constexpr uint16_t UsageToUsageId(uint32_t usage) { return static_cast<uint16_t>(usage); }

enum class Page : uint16_t {
  kUndefined = 0x00,
  kGenericDesktop = 0x01,
  kSimulationCtrls = 0x02,
  kVRCtrls = 0x03,
  kSportsCtrls = 0x04,
  kGameCtrls = 0x05,
  kGenericDeviceCtrls = 0x06,
  kKeyboardKeypad = 0x07,
  kLEDs = 0x08,
  kButton = 0x09,
  kOrdinal = 0x0a,
  kTelephony = 0x0b,
  kConsumer = 0x0c,
  kDigitizer = 0x0d,

  kPhysicalInterface = 0x0f,
  kUnicode = 0x10,

  kAlphanumericDisplay = 0x14,

  kSensor = 0x20,

  kMedicalInstrument = 0x40,

  kMonitor = 0x80,
  kMonitorEnumerated = 0x81,
  kVESACtrls = 0x82,
  kVESACommand = 0x83,
  kPowerDevice = 0x84,
  kBatterySystem = 0x85,

  kBarcodeScanner = 0x8c,
  kScale = 0x8d,
  kMagneticStripeReader = 0x8e,
  kPointOfSaleDevice = 0x8f,
  kCameraControl = 0x90,
  kArcadeControl = 0x91,

  kFidoAlliance = 0xf1d0,

  kVendorDefinedStart = 0xff00,
  kVendorDefinedEnd = 0xffff
};

enum class GenericDesktop : uint32_t {
  kUndefined = 0x00,
  kPointer = 0x01,
  kMouse = 0x02,

  kJoystick = 0x04,
  kGamePad = 0x05,
  kKeyboard = 0x06,
  kKeypad = 0x07,
  kMultiAxisController = 0x08,

  kX = 0x30,
  kY = 0x31,
  kZ = 0x32,
  kRx = 0x33,
  kRy = 0x34,
  kRz = 0x35,
  kSlider = 0x36,
  kDial = 0x37,
  kWheel = 0x38,
  kHatSwitch = 0x39,
  kCountedBuffer = 0x3a,
  kByteCount = 0x3b,
  kMotionWakeup = 0x3c,

  kVx = 0x40,
  kVy = 0x41,
  kVz = 0x42,
  kVbrx = 0x43,
  kVbry = 0x44,
  kVbrz = 0x45,
  kVno = 0x46,

  kSystemControl = 0x80,
  kSystemPowerDown = 0x81,
  kSystemSleep = 0x82,
  kSystemWakeUp = 0x83,
  kSystemContextMenu = 0x84,
  kSystemMainMenu = 0x85,
  kSystemAppMenu = 0x86,
  kSystemMenuHelp = 0x87,
  kSystemMenuExit = 0x88,
  kSystemMenuSelect = 0x89,
  kSystemMenuRight = 0x8a,
  kSystemMenuLeft = 0x8b,
  kSystemMenuUp = 0x8c,
  kSystemMenuDown = 0x8d,

  kDpadUp = 0x90,
  kDpadDown = 0x9a,
  kDpadRight = 0x9b,
  kDpadLeft = 0x9c
};

enum class LEDs : uint32_t {
  kUndefined = 0x00,
  kNumLock = 0x01,
  kCapsLock = 0x02,
  kScrollLock = 0x03,
  kCompose = 0x04,
  kKana = 0x05,
  kPower = 0x06,
  kShift = 0x07,
  kDoNotDisturb = 0x08,
  kMute = 0x09,
  kToneEnable = 0x0a,
  kHighCutFilter = 0x0b,
  kLowCutFilter = 0x0c,
  kEqualizerEnable = 0x0d,
  kSoundFieldOn = 0x0e,
  kSurroundFieldOn = 0x0f,
  kRepeat = 0x10,
  kStereo = 0x11,
  kSamplingRateDetect = 0x12,
  kSpinning = 0x13,
  kCAV = 0x14,
  kCLV = 0x15,
  kRecordingFormatDetect = 0x16,
  kOffHook = 0x17,
  kRing = 0x18,
  kMessageWaiting = 0x19,
  kDataMode = 0x1a,
  kBatteryOperation = 0x1b,
  kBatteryOK = 0x1c,
  kBatteryLow = 0x1d,
  kSpeaker = 0x1e,
  kHeadSet = 0x1f,
  kHold = 0x20,
  kMicrophone = 0x21,
  kCoverage = 0x22,
  kNightMode = 0x23,
  kSendCalls = 0x24,
  kCallPickup = 0x25,
  kConference = 0x26,
  kStandby = 0x27,
  kCameraOn = 0x28,
  kCameraOff = 0x29,
  kOnLine = 0x2a,
  kOffLine = 0x2b,
  kBusy = 0x2c,
  kReady = 0x2d,
  kPaperOut = 0x2e,
  kPaperJam = 0x2f,
  kRemote = 0x30,
  kForward = 0x31,
  kReverse = 0x32,
  kStop = 0x33,
  kRewind = 0x34,
  kFastForward = 0x35,
  kPlay = 0x36,
  kPause = 0x37,
  kRecord = 0x38,
  kError = 0x39,
  kUsageSelectedIndicator = 0x3a,
  kUsageInUseIndicator = 0x3b,
  kUsageMultiModeIndicator = 0x3c,
  kIndicatorOn = 0x3d,
  kIndicatorFlash = 0x3e,
  kIndicatorSlowBlink = 0x3f,
  kIndicatorFastBlink = 0x40,
  kIndicatorOff = 0x41,
  kFlashOnTime = 0x42,
  kSlowBlinkOnTime = 0x43,
  kSlowBlinkOffTime = 0x44,
  kFastBlinkOnTime = 0x45,
  kFastBlinkOffTime = 0x46,
  kUsageIndicatorColor = 0x47,
  kRed = 0x48,
  kGreen = 0x49,
  kAmber = 0x4a,
  kGenericIndicator = 0x4b,
  kSystemSuspend = 0x4c,
  kExternalPowerConnected = 0x4d
};

enum class Consumer : uint32_t {
  kUnassigned = 0x00,
  kConsumerControl = 0x01,
  kNumericKeyPad = 0x02,
  kProgrammableButtons = 0x03,

  kPlus10 = 0x20,
  kPlus100 = 0x21,
  kAM_PM = 0x22,

  kPower = 0x30,
  kReset = 0x31,
  kSleep = 0x32,
  kSleepAfter = 0x33,
  kSleepMode = 0x34,
  kIllumination = 0x35,
  kFunctionButtons = 0x36,

  kMenu = 0x40,
  kMenuPick = 0x41,
  kMenuUp = 0x42,
  kMenuDown = 0x43,
  kMenuLeft = 0x44,
  kMenuRight = 0x45,
  kMenuEscape = 0x46,
  kMenuValueIncrease = 0x47,
  kMenuValueDecrease = 0x48,

  kDataOnScreen = 0x60,
  kClosedCaption = 0x61,
  kClosedCaptionSelect = 0x62,
  kVCR_TV = 0x63,
  kBroadcastMode = 0x64,
  kSnapshot = 0x65,
  kStill = 0x66,

  kCameraAccessEnabled = 0x76,
  kCameraAccessDisabled = 0x77,
  kCameraAccessToggle = 0x78,

  kSelection = 0x80,
  kAssignSelection = 0x81,
  kModeStep = 0x82,
  kRecallLast = 0x83,
  kEnterChannel = 0x84,
  kOrderMovie = 0x85,
  kChannel = 0x86,
  kMediaSelection = 0x87,
  kMediaSelectComputer = 0x88,
  kMediaSelectTV = 0x89,
  kMediaSelectWWW = 0x8a,
  kMediaSelectDVD = 0x8b,
  kMediaSelectTelephone = 0x8c,
  kMediaSelectProgramGuide = 0x8d,
  kMediaSelectVideoPhone = 0x8e,
  kMediaSelectGames = 0x8f,
  kMediaSelectMessages = 0x90,
  kMediaSelectCD = 0x91,
  kMediaSelectVCR = 0x92,
  kMediaSelectTuner = 0x93,
  kQuit = 0x94,
  kHelp = 0x95,
  kMediaSelectTape = 0x96,
  kMediaSelectCable = 0x97,
  kMediaSelectSatellite = 0x98,
  kMediaSelectSecurity = 0x99,
  kMediaSelectHome = 0x9a,
  kMediaSelectCall = 0x9b,
  kChannelIncrement = 0x9c,
  kChannelDecrement = 0x9d,
  kMediaSelectSAP = 0x9e,

  kVCRPlus = 0xa0,
  kOnce = 0xa1,
  kDaily = 0xa2,
  kWeekly = 0xa3,
  kMonthly = 0xa4,

  kPlay = 0xb0,
  kPause = 0xb1,
  kRecord = 0xb2,
  kFastForward = 0xb3,
  kRewind = 0xb4,
  kScanNextTrack = 0xb5,
  kScanPreviousTrack = 0xb6,
  kStop = 0xb7,
  kEject = 0xb8,
  kRandomPlay = 0xb9,
  kSelectDisC = 0xba,
  kEnterDisc = 0xbb,
  kRepeat = 0xbc,
  kTracking = 0xbd,
  kTrackNormal = 0xbe,
  kSlowTracking = 0xbf,
  kFrameForward = 0xc0,
  kFrameBack = 0xc1,
  kMark = 0xc2,
  kClearMark = 0xc3,
  kRepeatFromMark = 0xc4,
  kReturnToMark = 0xc5,
  kSearchMarkForward = 0xc6,
  kSearchMarkBackwards = 0xc7,
  kCounterReset = 0xc8,
  kShowCounter = 0xc9,
  kTrackingIncrement = 0xca,
  kTrackingDecrement = 0xcb,

  kVolume = 0xe0,
  kBalance = 0xe1,
  kMute = 0xe2,
  kBass = 0xe3,
  kTreble = 0xe4,
  kBassBoost = 0xe5,
  kSurroundMode = 0xe6,
  kLoudness = 0xe7,
  kMPX = 0xe8,
  kVolumeUp = 0xe9,
  kVolumeDown = 0xea,

  kSpeedSelect = 0xf0,
  kPlaybackSpeed = 0xf1,
  kStandardPlay = 0xf2,
  kLongPlay = 0xf3,
  kExtendedPlay = 0xf4,
  kSlow = 0xf5,

  kBalanceRight = 0x150,
  kBalanceLeft = 0x151,
  kBassIncrement = 0x152,
  kBassDecrement = 0x153,
  kTrebleIncrement = 0x154,
  kTrebleDecrement = 0x155,

  kSpeakerSystem = 0x160,
  kChannelLeft = 0x161,
  kChannelRight = 0x162,
  kChannelCenter = 0x163,
  kChannelFront = 0x164,
  kChannelCenterFront = 0x165,
  kChannelSide = 0x166,
  kChannelSurround = 0x167,
  kChannelLowFreqEnhance = 0x168,
  kChannelTop = 0x169,
  kChannelUnknown = 0x16a,

  kAppLaunchButtons = 0x180,

  kGenericGUIAppControls = 0x200
};

// These are the values that Digitizer::kTouchscreenInputMode can
// take and their respective meanings.
enum class TouchScreenInputMode : uint32_t {
  kMouse = 0x00,
  kSingleInput = 0x01,
  kMultipleInput = 0x02,
  // kWindowsPrecisionTouchpad defined here:
  // https://docs.microsoft.com/en-us/windows-hardware/design/component-guidelines/windows-precision-touchpad-required-hid-top-level-collections
  kWindowsPrecisionTouchpad = 0x03,
};

enum class Digitizer : uint32_t {
  kUndefined = 0x00,

  kDigitizer = 0x01,
  kPen = 0x02,
  kLightPen = 0x03,
  kTouchScreen = 0x04,
  kTouchPad = 0x05,
  kWhiteBoard = 0x06,
  kCoordinateMeasuringMachine = 0x07,
  k3DDigitizer = 0x08,
  kStereoPlotter = 0x09,
  kArticulatedArm = 0x0a,
  kArmature = 0x0b,
  kMultiplePointDigitizer = 0x0c,
  kFreeSpaceWand = 0x0d,
  kTouchScreenConfiguration = 0x0E,

  kStylus = 0x20,
  kFinger = 0x22,
  kTouchScreenDeviceSettings = 0x23,

  kTipPressure = 0x30,
  kBarrelPressure = 0x31,
  kInRange = 0x32,
  kTouch = 0x33,
  kUntouch = 0x34,
  kTap = 0x35,
  kQuality = 0x36,
  kDataValid = 0x37,
  kTransducerIndex = 0x38,
  kTabletFunctionKeys = 0x39,
  kProgramChangeKeys = 0x3a,
  kBatteryStrength = 0x3b,
  kInvert = 0x3c,
  kXTilt = 0x3d,
  kYTilt = 0x3e,
  kAzimuth = 0x3f,
  kAltitude = 0x40,
  kTwist = 0x41,
  kTipSwitch = 0x42,
  kSecondaryTipSwitch = 0x43,
  kBarrelSwitch = 0x44,
  kEraser = 0x45,
  kTabletPick = 0x46,
  kConfidence = 0x47,
  kWidth = 0x48,
  kHeight = 0x49,

  kContactID = 0x51,
  kTouchScreenInputMode = 0x52,
  kContactCount = 0x54,
  kScanTime = 0x56,
  kSurfaceSwitch = 0x57,
  kButtonSwitch = 0x58,
};

enum class Sensor : uint32_t {
  kUndefined = 0x00,

  kAmbientLight = 0x41,
  kAccelerometer3D = 0x73,
  kGyrometer3D = 0x76,
  kMagnetometer = 0xC2,

  kSensorState = 0x201,
  kSensorStateUndefined = 0x801,
  kSensorStateReady = 0x802,
  kSensorStateNotAvailable = 0x803,
  kSensorStateNoData = 0x804,
  kSensorStateInitializing = 0x805,
  kSensorStateAccessDenied = 0x806,
  kSensorStateError = 0x807,

  kSensorEvent = 0x202,
  kSensorEventUnknown = 0x810,
  kSensorEventStateChanged = 0x811,
  kSensorEventPropertyChanged = 0x812,
  kSensorEventDataUpdated = 0x813,
  kSensorEventPollResponse = 0x814,
  kSensorEventChangeSensitivity = 0x815,
  kSensorEventRangeMaxReached = 0x816,
  kSensorEventRangeMinReached = 0x817,
  kSensorEventHighThresholdCrossUpward = 0x818,
  kSensorEventHighThresholdCrossDownward = 0x819,
  kSensorEventLowThresholdCrossUpward = 0x81A,
  kSensorEventLowThresholdCrossDownward = 0x81B,
  kSensorEventZeroThresholdCrossUpward = 0x81C,
  kSensorEventZeroThresholdCrossDownward = 0x81D,

  kAccelerationAxisX = 0x453,
  kAccelerationAxisY = 0x454,
  kAccelerationAxisZ = 0x455,
  kAngularVelocityX = 0x457,
  kAngularVelocityY = 0x458,
  kAngularVelocityZ = 0x459,
  kDistanceAxisX = 0x47A,
  kDistanceAxisY = 0x47B,
  kDistanceAxisZ = 0x47C,
  kTiltAxisX = 0x47F,
  kTiltAxisY = 0x480,
  kTiltAxisZ = 0x481,
  kMagneticFluxAxisX = 0x485,
  kMagneticFluxAxisY = 0x486,
  kMagneticFluxAxisZ = 0x487,
  kLightIlluminance = 0x4D1,
  kLightColorTemperature = 0x4D2,
  kLightChromaticity = 0x4D3,
  kLightChromaticityX = 0x4D4,
  kLightChromaticityY = 0x4D5,
  kLightConsumerIrSentenceReceive = 0x4D6,
  kLightInfraredLight = 0x4D7,
  kLightRedLight = 0x4D8,
  kLightGreenLight = 0x4D9,
  kLightBlueLight = 0x4DA,
  kLightUltravioletALight = 0x4DB,
  kLightUltravioletBLight = 0x4DC,
  kLightUltravioletIndex = 0x4DD,
};

enum class Telephony : uint32_t {
  kUndefined = 0x00,

  kPhoneMute = 0x2F,
};

enum class FidoAlliance : uint32_t {
  kUndefined = 0x00,

  kU2FAuthenticatorDevice = 0x01,

  kInputReportData = 0x20,
  kOutputReportData = 0x21,
};

}  // namespace usage
}  // namespace hid

inline bool operator==(uint16_t e, hid::usage::Page up) { return (static_cast<uint16_t>(up) == e); }

inline bool operator==(hid::usage::Page up, uint16_t e) { return (static_cast<uint16_t>(up) == e); }

inline bool operator==(uint32_t e, hid::usage::GenericDesktop gd) {
  return (static_cast<uint32_t>(gd) == e);
}

inline bool operator==(hid::usage::GenericDesktop gd, uint32_t e) {
  return (static_cast<uint32_t>(gd) == e);
}

inline bool operator==(uint32_t e, hid::usage::Digitizer d) {
  return (static_cast<uint32_t>(d) == e);
}

inline bool operator==(hid::usage::Digitizer d, uint32_t e) {
  return (static_cast<uint32_t>(d) == e);
}

inline bool operator==(uint32_t e, hid::usage::LEDs gd) { return (static_cast<uint32_t>(gd) == e); }

inline bool operator==(uint32_t e, hid::usage::Consumer gd) {
  return (static_cast<uint32_t>(gd) == e);
}

inline bool operator==(hid::usage::Consumer gd, uint32_t e) {
  return (static_cast<uint32_t>(gd) == e);
}

inline bool operator==(uint32_t e, hid::usage::Sensor s) { return (static_cast<uint32_t>(s) == e); }
inline bool operator==(hid::usage::Sensor s, uint32_t e) { return (static_cast<uint32_t>(s) == e); }

#endif  // SRC_UI_INPUT_LIB_HID_PARSER_INCLUDE_LIB_HID_PARSER_USAGES_H_
