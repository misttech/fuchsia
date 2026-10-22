# Video 4 Linux 2 (V4L2) Webcam Agent Skill

This [Agent Skill][agent-skills-spec] teaches an AI agent how to use a Webcam
supported by the [Video 4 Linux 2 framework][v4l2-framework-docs]. The framework
supports all [USB Video device Class (UVC)][usb-uvc-spec] webcams.

The skill uses the following command-line tools:

* [v4l2-ctl][v4l2-ctl-man]
* [ffmpeg][ffmpeg-man]
* [fuser][fuser-man]
* [lsusb][lsusb-man]

[agent-skills-spec]: https://agentskills.io/specification
[ffmpeg-man]: https://manpages.ubuntu.com/manpages/resolute/man1/ffmpeg.1.html
[fuser-man]: https://manpages.ubuntu.com/manpages/resolute/man1/fuser.1.html
[lsusb-man]: https://manpages.ubuntu.com/manpages/resolute/man8/lsusb.8.html
[usb-uvc-spec]: https://www.usb.org/document-library/video-class-v15-document-set
[v4l2-ctl-man]: https://manpages.ubuntu.com/manpages/resolute/man1/v4l2-ctl.1.html
[v4l2-framework-docs]: https://web.archive.org/web/20140221165356/https://www.kernel.org/doc/Documentation/video4linux/v4l2-framework.txt
