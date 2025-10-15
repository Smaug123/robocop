from dataclasses import dataclass
import sys
import subprocess
import argparse
import json
import tempfile
import os
from openai import OpenAI
from typing import Sequence
from pathlib import Path

from openai.types.shared_params import ResponseFormatJSONSchema, ReasoningEffort


@dataclass
class GitData:
    diff: str
    files_changed: Sequence[str]
    head_hash: str
    branch_name: str | None
    merge_base_hash: str
    repo_name: str
    remote_url: str | None


def get_git_diff(default_branch_name: str) -> GitData:
    """
    Get git diff of current working directory against the merge-base.

    Assumes the local working directory is clean; if it's not, you'll get uncommitted changes included too.

    Additionally returns a sequence of the files that were changed.

    Throws on any error conditions.
    """
    try:
        head_hash = (
            subprocess.check_output(["git", "rev-parse", "HEAD"])
            .strip()
            .decode("utf-8")
        )

        merge_base = (
            subprocess.check_output(
                ["git", "merge-base", "HEAD", default_branch_name],
                stderr=subprocess.STDOUT,
            )
            .strip()
            .decode("utf-8")
        )

        branch_name = (
            subprocess.check_output(["git", "branch", "--show-current"])
            .strip()
            .decode("utf-8")
            or None
        )

        # Get the diff against the merge-base
        diff = subprocess.check_output(
            ["git", "diff", "--no-ext-diff", "--unified=5", "--no-color", merge_base],
            stderr=subprocess.STDOUT,
        ).decode("utf-8")

        # Get the list of changed files
        changed_files = (
            subprocess.check_output(
                ["git", "diff", "--name-only", merge_base], stderr=subprocess.STDOUT
            )
            .decode("utf-8")
            .splitlines()
        )

        repo_name = Path(
            subprocess.check_output(["git", "rev-parse", "--show-toplevel"])
            .strip()
            .decode("utf-8")
        ).name

        # Get remote tracking URL if available
        remote_url = None
        if branch_name:
            try:
                # Get the remote name for this branch
                remote_name = (
                    subprocess.check_output(
                        ["git", "config", "--get", f"branch.{branch_name}.remote"],
                        stderr=subprocess.DEVNULL,
                    )
                    .strip()
                    .decode("utf-8")
                )

                if remote_name:
                    # Get the URL for this remote
                    remote_url = (
                        subprocess.check_output(
                            ["git", "remote", "get-url", remote_name],
                            stderr=subprocess.DEVNULL,
                        )
                        .strip()
                        .decode("utf-8")
                    )
            except subprocess.CalledProcessError:
                # Branch doesn't have a remote or other git error - that's fine
                pass

        return GitData(
            diff=diff,
            files_changed=changed_files,
            merge_base_hash=merge_base,
            head_hash=head_hash,
            branch_name=branch_name,
            repo_name=repo_name,
            remote_url=remote_url,
        )

    except subprocess.CalledProcessError as e:
        print(f"Error obtaining git diff: {e.output.decode('utf-8')}", file=sys.stderr)
        sys.exit(1)


def read_file(path: str) -> Sequence[str]:
    """
    Read a file and return its contents as a single string.

    This really returns an option type, but Python makes that kind of hard,
    so instead you get a sequence of length 0 or 1.

    Only returns the contents if the file exists and is encoded using the system default encoding.
    """
    try:
        with open(path, "r") as f:
            return [f.read()]
    except (FileNotFoundError, UnicodeDecodeError):
        return []


def create_batch_request(
    system_prompt: str,
    user_prompt: str,
    response_format: ResponseFormatJSONSchema,
    reasoning_effort: ReasoningEffort,
) -> dict:
    """Create a single batch request in the required format."""
    return {
        "custom_id": "robocop-review-1",
        "method": "POST",
        "url": "/v1/chat/completions",
        "body": {
            "model": "gpt-5-2025-08-07",
            "reasoning_effort": reasoning_effort,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "response_format": response_format,
        },
    }


def process_batch_request(
    client: OpenAI,
    response_format: ResponseFormatJSONSchema,
    reasoning_effort: ReasoningEffort,
    *,
    system_prompt: str,
    user_prompt: str,
    head_hash: str,
    merge_base: str,
    branch_name: str | None,
    repo_name: str,
    remote_url: str | None,
) -> str:
    """Process request using OpenAI batch API and return the batch ID."""
    # Create batch request
    batch_request = create_batch_request(
        system_prompt, user_prompt, response_format, reasoning_effort
    )

    # Write to temporary JSONL file
    with tempfile.NamedTemporaryFile(mode="w", suffix=".jsonl", delete=False) as f:
        f.write(json.dumps(batch_request) + "\n")
        batch_file_path = f.name

    try:
        # Upload the batch file
        with open(batch_file_path, "rb") as f:
            batch_input_file = client.files.create(file=f, purpose="batch")

        # Create the batch
        metadata = {
            "description": "robocop code review tool",
            "source_commit": head_hash,
            "target_commit": merge_base,
            "branch": branch_name or "<no branch>",
            "metadata_schema": "1",
            "repo_name": repo_name,
        }
        if remote_url:
            metadata["remote_url"] = remote_url

        batch = client.batches.create(
            input_file_id=batch_input_file.id,
            endpoint="/v1/chat/completions",
            completion_window="24h",
            metadata=metadata,
        )

        return batch.id

    finally:
        # Clean up temporary file
        if os.path.exists(batch_file_path):
            os.unlink(batch_file_path)


def main():
    parser = argparse.ArgumentParser(
        description="Robocop: AI-powered code review tool."
    )
    parser.add_argument(
        "--default-branch", type=str, default="main", help="Default branch name."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="If set, do not make any changes, just print what would be done.",
    )
    parser.add_argument("--api-key", type=str, required=True, help="OpenAI API key.")
    parser.add_argument(
        "--additional-prompt",
        type=str,
        default="",
        help="Additional context to add to the user prompt.",
    )
    parser.add_argument(
        "--include-files",
        type=str,
        nargs="*",
        default=[],
        help="Additional files to include as context.",
    )
    parser.add_argument(
        "--reasoning-effort",
        type=str,
        default="high",
        choices=["low", "medium", "high"],
        help="Reasoning effort level (low, medium, or high).",
    )
    parser.add_argument(
        "--batch",
        action="store_true",
        help="Use OpenAI batch processing API (default: False).",
    )

    args = parser.parse_args()

    # Convert string to ReasoningEffort type safely
    def get_reasoning_effort(effort_str: str) -> ReasoningEffort:
        if effort_str == "low":
            return "low"
        elif effort_str == "medium":
            return "medium"
        elif effort_str == "high":
            return "high"
        else:
            # This shouldn't happen due to argparse choices validation
            raise ValueError(f"Invalid reasoning effort: {effort_str}")

    reasoning_effort = get_reasoning_effort(args.reasoning_effort)

    git_data = get_git_diff(args.default_branch)
    if not git_data.diff.strip():
        print("No changes detected.")
        return
    if not git_data.files_changed:
        print("No changed files detected.")
        return

    system_prompt = """
<instructions><assistantBackground>You are a helpful assistant who is a very knowledgeable senior software engineer.</assistantBackground>

<taskHighLevelDescription>Your task is to review code changes for correctness and for obvious inconsistencies.</taskHighLevelDescription>

<task>
Scan for any typos, algorithmic errors (such as off-by-one errors), incorrect usages of library functions, contradictions between documentation and code, and other 'obvious' problems.

Use a simple JSON format, as described below, with a "reasoning" string, a "substantiveComments" boolean, and a "summary" string; don't give any output except the JSON object, although use the "reasoning" key as free text to discuss why you came to your answer.
Always provide the "reasoning" key; then provide a "substantiveComments" boolean; then, if your "substantiveComments" judgement is "true", additionally provide a "summary" field with a human-readable summary of the discrepancy (still in JSON).
(If there are no `substantiveComments`, just set "summary" to "n/a".)

Your answer will almost always be displayed to the user in a form that omits the "reasoning" key, so use it freely and be as verbose as you need, while keeping it a pure JSON string. The "summary" field, if `substantiveComments` is set, will be presented to the user, so keep that simple and easy to read.

If you find no issues, just say so by indicating that there are no substantive comments; for example, don't reply with `"substantiveComments": true` only to summarise the *correct* changes you found. Instead, set `"substantiveComments": false`.

Whenever you call out an issue in your "summary" output, include the exact file path and enough context to help the user locate the relevant code.

Keep your feedback concise and bullet-pointed.

If you're analysing code that's written in a strongly-typed language, don't provide feedback which the compiler will certainly catch.
For example, don't identify compile-time type- or syntax-errors in C# or F# which will fail the build whether or not you notice them.
Do identify syntax errors in languages like Python and Groovy, which often lack easy correctness-checking mechanisms; and do identify likely mistakes in syntax which *silently* have unexpected effects on semantics even in languages that are amenable to static analysis.
</task>

<advice>
* We only support Python at version 3.10 and above; you don't need to care about earlier Python compatibility.
</advice>

<exampleOutput>
{ "reasoning": "The algorithmic transformation of the main `foo` loop is routine and correct. The use of the `_.Result` syntax in F# is non-standard, but if this is an error, the compiler will flag it.", "substantiveComments": false, "summary": "n/a" }
</exampleOutput>

<exampleOutput>
{ "reasoning": "The change transforms the main `foo` loop by rewriting it as a recursive function with an accumulator. This builds the resulting list back-to-front, and the user has not reversed the order before returning the accumulated value. This change of order is visible later when the function `consume` iterates over the list.", "substantiveComments": true, "summary": "* The recursive `foo` in libs/MyProject/Program.fs reverses the list as it accumulates. This change of behaviour may be a bug." }
</exampleOutput>

<exampleOutput>
{ "reasoning": "The change transforms the main `foo` loop by rewriting it as a recursive function with an accumulator. This builds the resulting list back-to-front, and the user has not reversed the order before returning the accumulated value. But the resulting list is then immediately converted to an unordered set, so this change is not observable to other parts of the program. All the other changes in the diff are minor and are clearly correct.", "substantiveComments": false, "summary": "n/a" }
</exampleOutput>

<exampleOutput>
{ "reasoning": "The change uses the three-argument overload of the glibc function `openat`, but the code path with `create=true` supplies the `O_CREAT` flag. At runtime, arbitrary bytes from the stack will be used to populate the fourth argument to `openat`.", "substantiveComments": true, "summary": "* The new call to `openat` in IOLibrary/File.cs uses the three-argument overload of the glibc function `openat`. When `create=true`, the `O_CREAT` flag is used, which will cause glibc to fill in the fourth argument to `openat` with arbitrary bytes from the stack. See `man 2 openat`; this is very likely a critical bug." }
</exampleOutput>

<exampleOutput>
{ "reasoning": "A couple of simple typos and errors.", "substantiveComments": true, "summary": "* In libs/Foo/Program.fs, the string 'unnecessary' has been misspelled as 'unneccessary'.\n* In libs/Foo/Program.fs, the call to `logger.LogInformation` has three templated arguments `{Timestamp}`, `{Path}`, and `{Status}`, but the call only supplies two arguments.\n* In apps/Bar/Program.fs, the call to `String.StartsWith` is implicitly using the current culture. You almost certainly wanted to pass `StringComparison.Ordinal`." }
</exampleOutput>
</instructions>
"""

    additional_prompt = args.additional_prompt.strip()

    client = OpenAI(api_key=args.api_key)

    user_prompt = f"""Below is a git diff, and then the contents of the altered files after the diff was applied.
{additional_prompt}

DIFF BEGINS:
"""

    user_prompt += git_data.diff
    user_prompt += "\nDIFF ENDS\n\nFILE CONTENTS AFTER DIFF APPLIED (omits non-Unicode files and files deleted in the diff):\n\n"

    for file in git_data.files_changed:
        for contents in read_file(file):
            user_prompt += f"\n === {file} ===\n\n{contents}\n\n"

    if args.include_files:
        user_prompt += "\nADDITIONAL CONTEXT FILES:\n\n"
        for file in args.include_files:
            if file not in git_data.files_changed:
                for contents in read_file(file):
                    user_prompt += f"\n === {file} ===\n\n{contents}\n\n"

    if args.dry_run:
        print("System prompt:")
        print(system_prompt)
        print("\nUser prompt:")
        print(user_prompt)
        return

    response_format: ResponseFormatJSONSchema = {
        "type": "json_schema",
        "json_schema": {
            "schema": {
                "type": "object",
                "properties": {
                    "reasoning": {"type": "string"},
                    "substantiveComments": {"type": "boolean"},
                    "summary": {"type": "string"},
                },
                "required": ["reasoning", "substantiveComments", "summary"],
                "additionalProperties": False,
            },
            "strict": True,
            "name": "RobocopReview",
        },
    }

    if args.batch:
        # Use batch processing
        batch_id = process_batch_request(
            client,
            response_format,
            reasoning_effort,
            system_prompt=system_prompt,
            user_prompt=user_prompt,
            head_hash=git_data.head_hash,
            merge_base=git_data.merge_base_hash,
            branch_name=git_data.branch_name,
            repo_name=git_data.repo_name,
            remote_url=git_data.remote_url,
        )
        print(batch_id)
        return
    else:
        # Use regular chat completions API
        # The Chat endpoint supports only the `reasoning_effort` parameter, not the more detailed
        # `reasoning` parameter you get on the more powerful Responses endpoint.
        # See https://platform.openai.com/docs/api-reference/chat/create#chat_create-reasoning_effort .
        response = client.chat.completions.create(
            model="gpt-5-2025-08-07",
            reasoning_effort=reasoning_effort,
            messages=[
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            response_format=response_format,
        )

        if len(response.choices) != 1:
            print(
                f"Unexpected number of choices: {len(response.choices)}",
                file=sys.stderr,
            )
            sys.exit(1)

        message = response.choices[0]
        if message.finish_reason != "stop":
            print(f"Unexpected finish reason: {message.finish_reason}", file=sys.stderr)
            sys.exit(1)

        if not message.message.content:
            print("No content in message", file=sys.stderr)
            sys.exit(1)

        print(message.message.content.strip())


if __name__ == "__main__":
    main()
