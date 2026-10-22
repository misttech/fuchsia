### Advanced photo concepts for V4L2 webcams

#### Argument breakdown for webcam photo command

Command syntax:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i <DEVFS_ENTRY> -frames:v 1 -update 1 <FILE_PATH>
```

Arguments:

* `-hide_banner` eliminates a long `ffmpeg` build configuration

* `-loglevel error` eliminates potentially misleading logging output

* `-f <INPUT_FORMAT> -i <INPUT_FILE>` defines a media source; always specify
  `-f` rather than letting `ffmpeg` guess the format

* `-frames:v <FRAME_COUNT>` sets the number of frames in the output; must be
  placed between the input description and the output description to apply to
  the output

* `-update 1` tells the image sequence muxer to overwrite a single file instead
  of creating a numbered sequence of files

#### Wait for auto-exposure, auto-focus, etc. to stabilize

Approach: start video capture, skip the first few seconds, save a frame.

Argument syntax: `-ss <SKIP_SECONDS>`

Example:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i /dev/video0 -ss 2 -frames:v 1 -update 1 webcam-photo.png
```

#### Specify output resolution

Argument syntax: `-video_size <WIDTH>x<HEIGHT>`

Example:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -video_size 1920x1080 -i /dev/video0 -frames:v 1 -update 1 webcam-photo.png
```

#### Specify input pixel format

Argument syntax: `-input_format <PIXEL_FORMAT>`

Common `ffmpeg` pixel format names corresponding to `v4l2-ctl` formats:

* `YUYV` -> `yuyv422`
* `NV12` -> `nv12`
* `MJPG` -> `mjpeg`

Example:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -input_format mjpeg -i /dev/video0 -frames:v 1 -update 1 webcam-photo.png
```

#### Flip and rotate the photo

Approach: use FFmpeg video filters to transform the image.

Argument syntax: `-vf "<FILTER>"`

Available filters:

* `hflip` - horizontal flip
* `vflip` - vertical flip
* `transpose=1` - 90 degrees clockwise rotation
* `transpose=2` - 90 degrees counter-clockwise rotation

Multi-filter argument syntax: `-vf "<FILTER>,<FILTER>..."`

Example:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i /dev/video0 -vf "hflip,transpose=1" -frames:v 1 -update 1 webcam-photo.png
```
