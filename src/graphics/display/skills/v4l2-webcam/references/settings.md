### Managing V4L2 webcam settings

#### List settings

Command syntax: `v4l2-ctl --device <DEVFS_ENTRY> --list-ctrls-menus`

Example: `v4l2-ctl --device /dev/video0 --list-ctrls-menus`

The output lists the following information for each setting.

* name; example: `brightness`
* numerical ID; example: `0x00980900`
* value type; examples: `int`, `bool`, `menu`
* range information; example: `min=0 max=100 step=1`
* default and current value; example: `default=50 value=50`
* list of valid values (only for the `menu` type)

#### Read a setting's value

Command syntax: `v4l2-ctl --device <DEVFS_ENTRY> --get-ctrl <SETTING_ID>`

The setting ID can be the name or numerical ID.

Example: `v4l2-ctl --device /dev/video0 --get-ctrl brightness`

Example output: `brightness: 50`

#### Change a setting's value

Command syntax:

```sh
v4l2-ctl --device <DEVFS_ENTRY> --set-ctrl <SETTING_ID>=<VALUE>
```

Example: `v4l2-ctl --device /dev/video0 --set-ctrl brightness=64`

#### Settings for pan / tilt / zoom (PTZ)

Common PTZ setting names:

* `pan_absolute`
* `tilt_absolute`
* `zoom_absolute`
* `focus_absolute`

Example of zooming in:

* Setting name and type: `zoom_absolute 0x009a090d (int)`
* Setting range: `min=0 max=100 step=1 default=0 value=0`
* Command: `v4l2-ctl --device /dev/video0 --set-ctrl zoom_absolute=50`

#### Settings for exposure and post-processing

Common exposure setting names:

* `exposure_time_absolute`

Common post-processing setting names:

* `brightness`
* `contrast`
* `gain`
* `saturation`
* `sharpness`
* `white_balance_temperature`

#### Manual vs automated settings

When you plan to change a setting, check for an associated "automated" setting.
Disable the associated "automated" setting before changing the non-automated
setting.

Automated counterparts for the settings shown so far:

* `focus_absolute` -> `focus_automatic_continuous`
* `exposure_time_absolute` -> `auto_exposure`, `exposure_dynamic_framerate`
* `white_balance_temperature` -> `white_balance_automatic`,
  `white_balance_temperature_auto`

Example 1: You are planning to change `focus_absolute` (manual focus), and
you discovered a `focus_automatic_continuous` (auto-focus) setting with type
`bool`. Take the following steps.

1. Disable auto-focus:
   `v4l2-ctl --device /dev/video0 --set-ctrl focus_automatic_continuous=0`

2. Make the planned focus change:
   `v4l2-ctl --device /dev/video0 --set-ctrl focus_absolute=50`

3. If asked, re-enable auto-focus when done capturing content:
   `v4l2-ctl --device /dev/video0 --set-ctrl focus_automatic_continuous=1`

Example 2: You are planning to change `exposure_time_absolute`, and you
discovered an `auto_exposure` setting with type `menu`. Take the following
steps.

1. Determine the menu entry that is equivalent to "manual".

   * If you are not sure, conduct a Web search. Include the webcam model,
   setting name, and menu values.
   * Example manual value: "1: Manual Mode"
   * Example automated value: "3: Aperture Priority Mode"

2. Switch the setting:
   `v4l2-ctl --device /dev/video0 --set-ctrl auto_exposure=1`

3. Make the planned exposure change:
   `v4l2-ctl --device /dev/video0 --set-ctrl exposure_time_absolute=300`

4. If asked, re-enable auto-exposure when done capturing content:
   `v4l2-ctl --device /dev/video0 --set-ctrl auto_exposure=3`
