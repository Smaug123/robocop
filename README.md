# Robocop

Robocop is a dumb little bot that feeds diffs into a smart big bot for review.

# Components

* A [dashboard](./dashboard.html), which (given an OpenAI API key) scrapes the [batch API](https://platform.openai.com/docs/guides/batch) for Robocop reviews and displays them all.
* A [GitHub app](./github-bot), which watches pull requests to a GitHub repo and sends the diffs off for review.
* A [Python script](./python/robocop.py), which can be invoked manually to perform a review from your own machine.

## Dashboard

This was *entirely* written by Claude Sonnet 4.5, with review by GPT-5 at max reasoning.

## Bot

This was *entirely* written by Claude Sonnet 4.5, with review by GPT-5 at max reasoning.

## Python script

This is an unholy combination of me, GPT-5, and GPT-4.1 through GitHub Copilot, with later edits by Claude Sonnet 4.5.

# Licence

The prompt is adapted from G-Research's [Robocop](https://github.com/G-Research/robocop), and the Python script in the `python/` folder is also substantially adapted from G-Research's version; both are used under [the MIT licence](./prompt-LICENSE).

The project is licensed to you under the [MIT licence](./LICENSE).
