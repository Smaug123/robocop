# Robocop

Robocop is a dumb little bot that feeds diffs into a smart big bot for review.

# Components

* A [dashboard](./dashboard.html), which (given an OpenAI API key) scrapes the [batch API](https://platform.openai.com/docs/guides/batch) for Robocop reviews and displays them all.
* A [GitHub webhook server](./robocop-server), which watches pull requests to a GitHub repo and sends the diffs off for review.
* A [Rust CLI](./robocop-cli), which can be invoked manually to perform a review from your own machine.

## Dashboard

This was *entirely* written by Claude Sonnet 4.5, with review by GPT-5 at max reasoning.

## Server and CLI

The Rust components (robocop-core library, robocop-server, and robocop-cli) were written by Claude Sonnet 4.5, with review by GPT-5 at max reasoning.

# Licence

The prompt is adapted from G-Research's [Robocop](https://github.com/G-Research/robocop) and is used under [the MIT licence](./prompt-LICENSE).

The project is licensed to you under the [MIT licence](./LICENSE).
