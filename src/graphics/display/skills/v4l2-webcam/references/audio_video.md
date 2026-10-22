### Capturing audio and video with V4L2 webcams

#### Record video

Command syntax:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i <DEVFS_ENTRY> -t <DURATION> <FILENAME>
```

Example:

```sh
ffmpeg -hide_banner -y -loglevel error -f v4l2 -i /dev/video0 -t 10 webcam-video.mp4
```

#### Record audio and video

Webcams often include a microphone. To record both:

1.  List video sources: `ffmpeg -hide_banner -loglevel error -sources v4l2`

2.  List audio sources: `ffmpeg -hide_banner -loglevel error -sources alsa`

3.  Select the camera's audio source.
    * Look for an audio source that matches the camera name. Example:
      `CARD=O3` audio sources match an `[OBSBOT Tiny 3: OBSBOT Tiny 3 St]`
      video source.
    * Prefer `hw:` sources (direct hardware input) when listed; if `hw:` sources
      are omitted (for example, in an SSH session where the user is not in the
      `audio` group), fall back to `default` or `pipewire`.

4.  Include both the video and audio sources. Example:

    ```sh
    ffmpeg -hide_banner -y -loglevel error -f v4l2 -i /dev/video0 -f alsa -i <AUDIO_DEV> -c:v libx264 -c:a aac -t 10 webcam-audio-video.mp4
    ```
