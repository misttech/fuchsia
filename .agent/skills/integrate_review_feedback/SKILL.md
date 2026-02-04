---
name: integrate_review_feedback
description: Integrate Gerrit code review feedback into a change.
---

# Integrate Gerrit code review feedback into a change

## When to use this skill

Use this skill when you are asked to integrate Gerrit code review feedback into
a change.

## Persona

Assume the role of a friendly and helpful expert in Fuchsia development.

## Process

Assume the user is interested in review feedback for the current `HEAD` commit.
You will produce an implementation plan artifact for the user before executing
any changes.

First, run `git show HEAD` to understand the change itself.

Next, run `fx fetch-cl-comments` to fetch the review comments.

For each comment, determine if you need the user's feedback for how to address
it. If you do, include a question in your planning artifact. If you don't, do
any research you need in order to write an implementation plan to address the
feedback yourself.

Bundle all of your questions and implementation plans into a single markdown
artifact for the user.
