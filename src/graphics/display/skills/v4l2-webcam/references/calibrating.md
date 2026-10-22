### Manually calibrating a V4L2 webcam

Follow the process below when the user asks to determine the optimal settings
for a given task (or situation), or to calibrate the webcam for a task.

The process is involved. Delegate it to a subagent, if you have that capability.

#### Process steps

1. Read [the webcam settings guide](settings.md), if you haven't already.

2. Read [the advanced photos guide](photos.md), if you haven't already.

3. Decide on a set of criteria for determining the quality of a webcam photo.
   More guidelines in a section below.

4. List the webcam's V4L2 settings, including options for menu-based settings.
   Pay attention to each setting's name, type, range, and current value.

5. Classify the settings as "automated" or "manual" controls.
   Example: auto-exposure vs exposure time.

6. Write a draft calibration script to track what you learned. More guidelines
   in a section below.

7. Verify that the draft script has *all* the setting names listed in the
   `v4l2-ctl` output.

8. Run the draft calibration script.

9. Decide on an order for adjusting "manual" settings. More guidelines in a
   section below.

10. Calibrate the settings iteratively. For each manual setting:

    1. Decide on a method for sampling the entire valid ranges of values.
       Use your knowledge of the setting's semantics to decide if you'll try
       all the values, do uniform sampling, or follow a more complex strategy
       (like logarithmic sampling).

    2. Write and execute a script that captures a photo for each sample value of
       the setting. Example:
       [uniform sampling stage in Bash](../assets/uniform_sampling.sh)

    3. Compare the sampled photos. Use the quality criteria you picked earlier
       to decide on the best value for the setting.

    4. Update the calibration script with the value you picked. Change the
       setting's status to reflect your confidence in the value. Status
       examples: "Fully calibrated.", "Calibration precision: 100".

    5. Consider reordering the settings that you're adjusting, or even
       revisiting an earlier setting.

11. Verify that the calibration script states that all settings are calibrated.
    At a minimum, do a text search for "Status: not calibrated".

12. You can now use the calibration script to apply the settings you discovered
    any time you need them.

#### Step 3: Webcam photo quality criteria

First, consider how the photos meet the user's end goal. If you don't have a
clear answer, ask the user directly.

Second, rank criteria based on the goal.

Example: Suppose the webcam is used to verify a device's display. Prioritize

1. Text clarity - can you read text from the display?
2. Image clarity - can you tell apart any shapes?
3. Color accuracy - can you distinguish similar colors?

#### Step 6: Draft calibration script guidelines

Example: [first draft calibration in Bash](../assets/draft_calibration.sh).

Language alternatives: If you can't execute Bash, translate the example to any
other scripting language that you can write and execute.

Scope: Cover **all** the setting names output by `v4l2-ctl --list-ctrls-menus`.

Values: For each "automatic" setting, record the value that disables automated
adjustments. For each "manual" setting, record the current value.

Type information: For each setting, include type and range information in a
comment.  For each menu setting, also include the description of the current
value in the comment.

Status: For each setting, include the status in a comment. In the first draft,
the status is "disabling automated adjustments" or "not calibrated".

#### Step 9: Ordering "manual" settings to be adjusted

1. Make an inventory of interactions between settings.

Example: You are optimizing for text clarity. Consider that focus and
exposure interact. Text blooming makes it impossible to assess focus in an
over-exposed photo. A completely out-of-focus photo makes it impossible to
assess exposure.

2. Use multiple iterations for settings with large ranges of values.

Example: A white balance setting has values between 1 and 5000, and a step size
of 5. Use three iterations, where the sampling steps are 500, then 50, then 5.
The last iteration matches the setting's step size.

3. Plan to iterate on interacting settings together.

Example: You are optimizing for text clarity. Start with a rough focus
optimization, to get the text somewhat legible. Optimize exposure, to avoid text
blooming. Fine-tune the focus, then further fine-tune the exposure.
