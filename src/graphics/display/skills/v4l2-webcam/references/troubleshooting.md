### V4L2 webcam troubleshooting steps

#### ffmpeg installation

Run the command `which ffmpeg` to check if the PATH includes the tool.

If the result is empty, offer to run the command `sudo apt install ffmpeg`.

#### ffmpeg full manual

If `ffmpeg` does not behave as expected, run the following command
and read the output to get an up-to-date usage guide.

```sh
ffmpeg -hide_banner -loglevel error --help full
```

#### v4l2-ctl installation

Run the command `which v4l2-ctl` to check if the PATH includes the tool.

If the result is empty, offer to run the command `sudo apt install v4l-utils`.

#### v4l2-ctl full manual

If `v4l2-ctl` does not behave as expected, run `v4l2-ctl --help-all` and read
the output to get an up-to-date usage guide.

#### Missing camera

If `v4l2-ctl --list-devices` does not show any webcam, run `lsusb` to learn if
the user has a Webcam without UVC support.

#### Device or resource busy

If a command fails with an error message similar to "Device or resource busy",
run `fuser --all --verbose <DEVFS_ENTRY>` to check if another
application is currently using the webcam.

If the command indicates a process is using the webcam, offer to run the
following command to kill the owning process: `fuser --kill -9 <DEVFS_ENTRY>`.

Example command: `fuser --all --verbose /dev/video0`

Example output indicating another process is using the webcam:

```
                     USER        PID ACCESS COMMAND
/dev/video0:         costan    2160121 F...m ffmpeg
```

Example command for killing the process: `fuser --kill -9 /dev/video0`
