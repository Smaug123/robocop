from dataclasses import dataclass
import sys
import subprocess
import argparse
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


def get_git_diff(default_branch_name: str) -> GitData:
    """
    Get git diff of current working directory against the merge-base.

    Assumes the local working directory is clean; if it's not, you'll get uncommitted changes included too.

    Throws on all error conditions.
    """
    try:
        head_hash = (
            subprocess.check_output(["git", "rev-parse", "HEAD"])
            .strip()
            .decode("utf-8")
        )
        merge_base = (
            subprocess.check_output(["git", "merge-base", "HEAD", default_branch_name])
            .strip()
            .decode("utf-8")
        )
        branch_name = (
            subprocess.check_output(["git", "branch", "--show-current"])
            .strip()
            .decode("utf-8")
        )
        diff = (
            subprocess.check_output(
                [
                    "git",
                    "diff",
                    "--no-ext-diff",
                    "--unified=5",
                    "--no-color",
                    merge_base,
                ]
            )
            .strip()
            .decode("utf-8")
        )
        changed_files = (
            subprocess.check_output(["git", "diff", "--name-only", merge_base])
            .strip()
            .decode("utf-8")
            .splitlines()
        )

        repo_name = Path(
            subprocess.check_output(["git", "rev-parse", "--show-toplevel"])
            .strip()
            .decode("utf-8")
        ).name

        return GitData(
            diff=diff,
            files_changed=changed_files,
            merge_base_hash=merge_base,
            head_hash=head_hash,
            branch_name=branch_name or None,
            repo_name=repo_name,
        )

    except subprocess.CalledProcessError as e:
        print(f"Error obtaining git diff: {e.output.decode('utf-8')}", file=sys.stderr)
        raise


def read_file(path: str) -> Sequence[str]:
    """
    Read a file and (if we successfully read the file) return its contents.

    The resulting Sequence is pretending to be Optional[str], but it's more ergonomic to consume this way:
    the sequence has length 0 or 1. That is, on success, you get a single-element sequence which contains
    the entire contents of the file as its one element.

    Returns the 0-element sequence for non-Unicode files; you only get readable text files in the sequence.
    """
    try:
        with open(path, encoding="utf-8") as f:
            return (f.read(),)
    except (UnicodeDecodeError, FileNotFoundError):
        return ()


def main():
    parser = argparse.ArgumentParser(
        description="Perform a code review of the diff against the merge-base with the main branch.",
        formatter_class=argparse.RawTextHelpFormatter,
    )
    parser.add_argument(
        "--default-branch",
        type=str,
        default="main",
        help="Name of the default branch of the repo.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the outgoing message to stdout, then exit, without invoking the LLM",
    )
    parser.add_argument(
        "--additional-prompt",
        type=str,
        default="",
        help="Additional context to pass to the LLM in the user prompt.",
    )
    parser.add_argument(
        "--include-files",
        type=str,
        nargs="*",
        default=[],
        help="Additional files to include as context in the user prompt.",
    )
    parser.add_argument(
        "--reasoning-effort",
        type=str,
        default="high",
        choices=["low", "medium", "high"],
        help="Reasoning effort to request of the bot.",
    )

    args = parser.parse_args()

    def get_reasoning_effort(effort: str) -> ReasoningEffort:
        if effort == "low":
            return "low"
        if effort == "medium":
            return "medium"
        if effort == "high":
            return "high"
        raise ValueError(f"Invalid reasoning effort: {effort}")

    reasoning_effort = get_reasoning_effort(args.reasoning_effort)
    git_data = get_git_diff(args.default_branch)
    if not git_data.diff.strip():
        print("No changes detected.")
        return
    if not git_data.files_changed:
        print("No changed files detected.")
        return

    system_prompt = """<instructions><assistantBackground>You are a helpful assistant who is a very knowledgeable senior software engineer.</assistantBackground>

<taskHighLevelDescription>Your task is to review code changes for correctness and for obvious inconsistencies.</taskHighLevelDescription>

<task>
Scan for any typos, algorithmic errors (such as off-by-one errors), incorrect usages of library functions, contradictions between documentation and code, and other 'obvious' problems.

Use a simple JSON format, as described below, with a "reasoning" string, a "substantiveComments" boolean, and a "summary" string; don't give any output except the JSON object, although use the "reasoning" key as free text to discuss why you came to your answer.
Always provide the "reasoning" key; then provide a "substantiveComments" boolean; then, if your "substantiveComments" judgement is "true", additionally provide a "summary" field with a human-readable summary of the discrepancy (still in JSON).
(If there are no `substantiveComments`, just set "summary" to "n/a".)

Your answer will almost always be displayed to the user in a form that omits the "reasoning" key, so use it freely and be as verbose as you need, while keeping it a pure JSON string. The "summary" field, if `substantiveComments` is set, will be presented to the user, so keep that simple and easy to read.
Any output displayed to the user should be formatted using GitHub Flavoured Markdown (serialised into the JSON string).

If you find no issues, just say so by indicating that there are no substantive comments: for example, don't reply with `"substantiveComments": true` only to summarise the *correct* changes you found. Instead, set `"substantiveComments": false`.

Whenever you call out an issue in your "summary" output, include the exact file path and enough context to help the user locate the relevant code.

Keep your feedback concise and bullet-pointed.

If you're analysing code that's written in a strongly-typed compiled language, don't provide feedback which the compiler will certainly catch.
For example, don't identify compile-time type- or syntax-errors in C# or F# which will fail the build whether or not you notice them.
Do identify syntax errors in languages like Python and Groovy, which often lack easy correctness-checking mechanisms; and do identify likely mistakes in syntax which *silently* have unexpected effects on semantics even in languages that are amenable to static analysis.
</task>

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
{ "reasoning": "The change uses the three-argument overload of the glibc function `openat`, but the code path with `create=true` supplies the `O_CREAT` flag. At runtime, arbitrary bytes from the stack will be used to populate the fourth argument to `openat`.", "substantiveComments": true, "summary": "* The new call to `openat` in FileHandling/File.cs uses the three-argument overload of the glibc function `openat`. When `create=true`, the `O_CREAT` flag is used, which will cause glibc to fill in the fourth argument to `openat` with arbitrary bytes from the stack. See `man 2 openat`; this is very likely a critical bug." }
</exampleOutput>

<exampleOutput>
{ "reasoning": "A couple of simple typos and errors.", "substantiveComments": true, "summary": "* In libs/Foo/Program.fs, the string 'unnecessary' has been misspelled as 'unneccessary'.\\n* In libs/Foo/Program.fs, the call to `logger.LogInformation` has three templated arguments `{Timestamp}`, `{Path}`, and `{Status}`, but the call only supplies two arguments.\\n* In apps/Bar/Program.fs, the call to `String.StartsWith` is implicitly using the current culture. You almost certainly wanted to pass `StringComparison.Ordinal`." }
</exampleOutput>
</instructions>
"""

    additional_prompt = args.additional_prompt.strip()

    user_prompt = f"""Below is a git diff, and then the contents of the altered files after the diff was applied.
{additional_prompt}

DIFF BEGINS:
"""

    user_prompt += git_data.diff
    user_prompt += "\nDIFF ENDS\n\nFILE CONTENTS AFTER DIFF APPLIED (omits non-Unicode files and files deleted in the diff):\n\n"

    file_content: str = "\n\n".join(
        [
            f"=== {file} ===\n\n{r}"
            for file in git_data.files_changed
            for r in read_file(file)
        ]
    )

    user_prompt += file_content

    if args.include_files:
        user_prompt += "\nADDITIONAL CONTEXT FILES:\n\n"
        for file in args.include_files:
            if file not in git_data.files_changed:
                for contents in read_file(file):
                    user_prompt += f"\n === {file} ===\n\n{contents}\n\n"

    if args.dry_run:
        print("System prompt:")
        print(system_prompt)
        print("User prompt:")
        print(user_prompt)
        return

    model = "gpt-5-2025-08-07"

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
            "name": "reasoningAndSummary",
        },
    }

    # API key read automatically from $OPENAI_API_KEY

    client = OpenAI()

    # Chat endpoint supports only the `reasoning_effort` parameter rather than the more detailed `reasoning` parameter you get
    # from the more powerful Responses endpoint.
    # See https://platform.openai.com/docs/api-reference/chat/create#chat_create-reasoning_effort .
    # Works only on OpenAI-API-compatible models and is ignored on e.g. qwen-coder; documented as supporting "minimal", "low",
    # "medium", and "high", but in fact I have observed "minimal" to give a "bad request" response.
    response = client.chat.completions.create(
        model=model,
        reasoning_effort=reasoning_effort,
        response_format=response_format,
        messages=[
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt},
        ],
    )

    if len(response.choices) != 1:
        print(f"Unexpected number of choices returned! {len(response.choices)}")
        for choice in response.choices:
            print(choice)
        sys.exit(2)

    choice = response.choices[0]
    if choice.finish_reason != "stop":
        print(f"Unexpectedly got non-stop finish reason {choice.finish_reason}")
        print(choice.message.content)
        sys.exit(3)

    if not choice.message.content:
        print("<no LLM output>")
    else:
        print(choice.message.content.strip())


if __name__ == "__main__":
    main()
