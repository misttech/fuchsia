---
name: v4l2-webcam
description: Webcam access - configure (pan/tilt/zoom), take photos and videos.
compatibility: Linux host, webcam with V4L2 support
---

# Webcam usage AI agent guide

## Basic commands

If any of the commands below does not work as expected, follow the
[Troubleshooting steps](references/troubleshooting.md).

### List available video sources

Command syntax: `ffmpeg -hide_banner -loglevel error -sources v4l2`

The output includes devfs paths and device names.

Example output:

```
Auto-detected sources for video4linux2,v4l2:
  /dev/video1 [OBSBOT Tiny 3: OBSBOT Tiny 3 St] (none)
  /dev/video0 [OBSBOT Tiny 3: OBSBOT Tiny 3 St] (none)
```

### Get webcam information

#### List available webcams and devfs entries

Command syntax: `v4l2-ctl --list-devices`

The output includes a camera name, USB device information, and paths to devfs
entries provided by the camera's driver.

Each webcam may be associated with multiple `/dev/videoX` devfs entries.

Usually, the lowest-numbered devfs entry (example: `/dev/video0`) for a webcam
provides video capture, and the other entries provide metadata. Use the command
below to verify if the user's webcam follows this convention.

#### List webcam capabilities

Command syntax: `v4l2-ctl --device <DEVFS_ENTRY> --info`

The output includes low-level information about the webcam driver and the
capabilities exposed by the devfs entry.

Most importantly, devfs entries that provide video capture list `Video Capture`
under `Device Caps`.

#### List webcam pixel formats and resolutions

Command syntax: `v4l2-ctl --device <DEVFS_ENTRY> --list-formats-ext`

Example: `v4l2-ctl --device /dev/video0 --list-formats-ext`

Output information hierarchy: pixel format > frame size (resolution) > interval
(frame rate).

Popular pixel formats are `YUYV` (raw), `NV12` (downsampled) and `MJPG`
(compressed). Compressed pixel formats may support higher frame rates at high
resolutions.

### Take a photo with the webcam

Approach: save a single video frame as an image.

Command syntax:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i <DEVFS_ENTRY> -frames:v 1 -update 1 <FILE_PATH>
```

Example:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i /dev/video0 -frames:v 1 -update 1 webcam-photo.png
```

#### Check photo file creation

Command syntax: `file <FILE_PATH>`

Example: `file webcam-photo.png`

Example successful output:

```
webcam-photo.png: PNG image data, 1920 x 1080, 8-bit/color RGB, non-interlaced
```

#### Assess photo quality

Read the captured image file into an AI model with vision capabilities to check
that the lighting, framing, and focus are appropriate for the user's end goal.

#### Misleading error messages

A bug in FFmpeg's V4L2 teardown sequence causes the following false-positives to
be logged to stderr:

* `Some buffers are still owned by the caller on close.`
* `ioctl(VIDIOC_QBUF): Bad file descriptor`

Ignore these messages. Use the `file` command described above to check for the
command's success.

## Advanced topics

Read the sections below when the basic commands above aren't sufficient.

* [Control webcam settings](references/settings.md)
* [Calibrate the webcam](references/calibrating.md) - determine optimal settings for a situation
* [Advanced photo concepts](references/advanced_photos.md)
* [Capture audio and video](references/audio_video.md)
