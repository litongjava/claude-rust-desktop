---
name: loop
description: "Run a prompt or check repeatedly on an interval and stop when the goal is met"
user-invocable: true
disable-model-invocation: false
---
Run this prompt or check repeatedly: $ARGUMENTS

1. State the repeated action, how often it should run, and the condition that
   ends the loop. If the user gave no stop condition, ask for one with
   AskUserQuestion before scheduling.
2. For a self-paced loop in this session, use ScheduleWakeup to set the next
   wake-up with a short reason. For a fixed schedule, use CronCreate; choose
   the cron expression deliberately and say whether the job is session-only or
   durable.
3. Keep each iteration short: do the check or action, record the result,
   decide whether the stop condition holds, and only then schedule the next
   wake-up. Avoid repeating long work on every iteration.
4. List active jobs with CronList and remove scheduled jobs with CronDelete by
   ID when the goal is reached, when the user asks, or when the job no longer
   applies.
5. Stop immediately on a clear success, an unrecoverable failure or the user's
   request; do not keep a loop alive to appear busy.

Report each iteration's observed result and the final state: goal reached, or
stopped early with the reason. Never leave a durable job behind without
telling the user.
