#!/usr/bin/env python3
"""Executable Python port of the Dify workflow defined in ao.yml."""

from __future__ import annotations

import asyncio
import argparse
import json
import os
import re
import sys
import unicodedata
import urllib.error
import urllib.request
from typing import Any


DEFAULT_OPENAI_BASE_URL = "https://api.openai.com/v1"
DEFAULT_OPENAI_MODEL = "gpt-4o-mini"
DEFAULT_OPENAI_TEMPERATURE = 0.7


VENDOR_PROMPT_TEMPLATE = """# 役割
経費精算の監査エージェント

# 比較対象
- OCR読み取り値: {ocr_vendor}
- ユーザー申請値: {user_vendor}

# 判定ルール
以下の優先順位で判定し、最も適切なものを1つ選んでください。

1. 【〇】（完全一致・実質一致）
   - 文字が完全に一致する場合。
   - 「支店名・店舗名（〇〇店、〇〇支店など）」の有無だけの違いである場合。
   - 中黒（・）、スペース、ハイフンなどの記号の有無だけの違いである場合。

2. 【〇（許容）】（表記揺れ・略称）
   - 株式会社や(株)など、法人格の有無や違い。
   - 英字表記とカタカナ表記の違い（例：「Amazon」と「アマゾン」）。
   - 事業者名の「略称」や「一部省略」である場合（例：「スターバックスコーヒー」と「スターバックス」）。
   - 全角・半角の違い、または明らかな誤字脱字。

3. 【アラート】
   - 全く異なる企業・ブランド名である場合、または一方が空欄の場合。

# 出力要件（絶対厳守）
あなたの出力は、必ず「〇」「〇（許容）」「アラート」のいずれか（文字列のみ）でなければなりません。理由や説明は絶対に記述しないでください。"""


INVOICE_PROMPT_TEMPLATE = """# 役割
インボイス登録番号の判定エージェント

# 判定ルール
入力データの組み合わせが、以下のパターンのどれに該当するかを確認し、指定された結果を出力してください。

※OCR結果が空っぽ（文字がない）場合は、「空欄」として扱います。

| パターン | 申請内容 | OCR結果に含まれるもの | 出力すべき結果 |
| :--- | :--- | :--- | :--- |
| パターン1 | 番号あり | T＋13桁の数字、または13桁の連続した数字 | 〇 |
| パターン2 | 番号なし | 空欄、または明らかな読み取り失敗 | 〇 |
| パターン3 | 番号あり | 空欄、または明らかな読み取り失敗 | アラート |
| パターン4 | 番号なし | T＋13桁の数字、または13桁の連続した数字 | アラート |

# 出力例
入力: OCR結果「T1234567890123」, 申請内容「番号あり」
出力: 〇

入力: OCR結果「」, 申請内容「番号あり」
出力: アラート

## 厳守事項
出力は「〇」または「アラート」の1語のみ。
理由・説明・記号・改行は一切含めないこと。

# 入力データ
OCR結果「{ocr_invoice_no}」, 申請内容「{user_invoice_flag}」
出力:"""


class WorkflowError(RuntimeError):
    """Raised when the workflow cannot complete as expected."""


def sanitize(value: Any) -> str:
    if value is None:
        return ""
    if not isinstance(value, str):
        value = str(value)
    value = unicodedata.normalize("NFKC", value)
    value = re.sub(r"[\t\r\n\x00-\x1f\x7f]", " ", value)
    value = re.sub(r" {2,}", " ", value)
    return value.strip()


def preprocess(
    content: str,
    user_amount: str = "",
    user_date: str = "",
    user_vendor: str = "",
    user_invoice_flag: str = "",
) -> dict[str, str]:
    try:
        ocr_data = json.loads(content)
    except json.JSONDecodeError:
        ocr_data = {}

    fields = ocr_data.get("fields", {})
    total = fields.get("total", {})
    raw_ocr_amount = str(total.get("amount", "")) if total else ""
    raw_ocr_date = fields.get("transactionDate", "")
    raw_ocr_vendor = fields.get("merchantName", "")
    raw_ocr_invoice_no = fields.get("merchantNumber", "")

    ocr_invoice_no_clean = sanitize(raw_ocr_invoice_no)
    ocr_invoice_flag = "番号あり" if ocr_invoice_no_clean else "番号なし"

    return {
        "ocr_amount": sanitize(raw_ocr_amount),
        "user_amount": sanitize(user_amount),
        "ocr_date": sanitize(raw_ocr_date),
        "user_date": sanitize(user_date),
        "ocr_vendor": sanitize(raw_ocr_vendor),
        "user_vendor": sanitize(user_vendor),
        "ocr_invoice_no": ocr_invoice_no_clean,
        "user_invoice_flag": sanitize(user_invoice_flag),
        "ocr_invoice_flag": ocr_invoice_flag,
    }


def to_integer(value: str) -> int | None:
    if not value or not str(value).strip():
        return None
    value = unicodedata.normalize("NFKC", str(value))
    digits_only = re.sub(r"[^\d]", "", value)
    if not digits_only:
        return None
    return int(digits_only)


def amount_check(ocr_amount: str, user_amount: str) -> str:
    if not ocr_amount or not str(ocr_amount).strip():
        return "アラート"
    if not user_amount or not str(user_amount).strip():
        return "アラート"

    ocr_val = to_integer(ocr_amount)
    user_val = to_integer(user_amount)

    if ocr_val is None or user_val is None:
        return "アラート"
    if ocr_val == user_val:
        return "○"
    return "アラート"


ERA_OFFSET = {
    "令和": 2018,
    "R": 2018,
    "平成": 1988,
    "H": 1988,
    "昭和": 1925,
    "S": 1925,
    "大正": 1911,
    "T": 1911,
}


def parse_date(value: str) -> tuple[int, int, int] | None:
    if not value or not str(value).strip():
        return None

    value = unicodedata.normalize("NFKC", str(value)).strip()

    era_pattern = re.compile(
        r"(令和|平成|昭和|大正|[RrHhSsTt])\s*(元|\d{1,2})\s*[年./]?\s*(\d{1,2})\s*[月./\-]?\s*(\d{1,2})\s*日?",
        re.IGNORECASE,
    )
    match = era_pattern.search(value)
    if match:
        era_str, year_str, month_str, day_str = match.groups()
        era_key = era_str.upper() if len(era_str) == 1 else era_str
        offset = ERA_OFFSET.get(era_key)
        if offset is None:
            return None
        year_num = 1 if year_str == "元" else int(year_str)
        year = offset + year_num
        return (year, int(month_str), int(day_str))

    western_pattern = re.compile(r"(\d{2,4})\s*[/\-\.年]\s*(\d{1,2})\s*[/\-\.月]\s*(\d{1,2})\s*日?")
    match = western_pattern.search(value)
    if match:
        year_str, month_str, day_str = match.groups()
        year = int(year_str)
        if year < 100:
            year += 2000
        return (year, int(month_str), int(day_str))

    return None


def date_check(ocr_date: str, user_date: str) -> str:
    if not ocr_date or not str(ocr_date).strip():
        return "注意"
    if not user_date or not str(user_date).strip():
        return "注意"

    ocr_parsed = parse_date(ocr_date)
    user_parsed = parse_date(user_date)

    if ocr_parsed is None or user_parsed is None:
        return "注意"
    if ocr_parsed == user_parsed:
        return "○"
    return "注意"


def extract_response_text(payload: dict[str, Any]) -> str:
    if isinstance(payload.get("output_text"), str) and payload["output_text"].strip():
        return payload["output_text"].strip()

    output = payload.get("output", [])
    for item in output:
        contents = item.get("content", [])
        for content in contents:
            text = content.get("text")
            if isinstance(text, str) and text.strip():
                return text.strip()
            if content.get("type") == "output_text" and isinstance(content.get("text"), str):
                text = content["text"].strip()
                if text:
                    return text

    raise WorkflowError(f"OpenAI response did not contain text: {json.dumps(payload, ensure_ascii=False)}")


def normalize_label(raw_text: str, allowed_labels: list[str]) -> str | None:
    text = sanitize(raw_text)
    if text in allowed_labels:
        return text

    ordered_labels = sorted(allowed_labels, key=len, reverse=True)
    hits = [label for label in ordered_labels if label in text]
    unique_hits = []
    for label in hits:
        if label not in unique_hits:
            unique_hits.append(label)

    if len(unique_hits) == 1:
        return unique_hits[0]

    return None


def call_openai_classifier(prompt: str, allowed_labels: list[str], request_name: str) -> str:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        raise WorkflowError("OPENAI_API_KEY is not set.")

    model = os.environ.get("OPENAI_MODEL", DEFAULT_OPENAI_MODEL)
    base_url = os.environ.get("OPENAI_BASE_URL", DEFAULT_OPENAI_BASE_URL).rstrip("/")
    temperature = float(os.environ.get("OPENAI_TEMPERATURE", str(DEFAULT_OPENAI_TEMPERATURE)))

    retry_suffix = ""
    for attempt in range(2):
        body = {
            "model": model,
            "instructions": prompt + retry_suffix,
            "input": "判定を実行してください。",
            "temperature": temperature,
            "max_output_tokens": 32,
        }
        data = json.dumps(body).encode("utf-8")
        request = urllib.request.Request(
            url=f"{base_url}/responses",
            data=data,
            headers={
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            },
            method="POST",
        )

        try:
            with urllib.request.urlopen(request) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            details = exc.read().decode("utf-8", errors="replace")
            raise WorkflowError(f"{request_name} OpenAI API error ({exc.code}): {details}") from exc
        except urllib.error.URLError as exc:
            raise WorkflowError(f"{request_name} OpenAI API connection error: {exc}") from exc

        raw_text = extract_response_text(payload)
        normalized = normalize_label(raw_text, allowed_labels)
        if normalized is not None:
            return normalized

        retry_suffix = "\n\n# 再指示\n出力は " + " / ".join(f"「{label}」" for label in allowed_labels) + " のいずれか1語のみ。"

    raise WorkflowError(f"{request_name} returned an unexpected label.")


def vendor_check(ocr_vendor: str, user_vendor: str) -> str:
    prompt = VENDOR_PROMPT_TEMPLATE.format(
        ocr_vendor=ocr_vendor or "",
        user_vendor=user_vendor or "",
    )
    return call_openai_classifier(prompt, ["〇（許容）", "〇", "アラート"], "vendor_check")


def invoice_check(ocr_invoice_no: str, user_invoice_flag: str) -> str:
    prompt = INVOICE_PROMPT_TEMPLATE.format(
        ocr_invoice_no=ocr_invoice_no or "",
        user_invoice_flag=user_invoice_flag or "",
    )
    return call_openai_classifier(prompt, ["〇", "アラート"], "invoice_check")


async def run_workflow_async(inputs: dict[str, Any]) -> dict[str, str]:
    content = inputs.get("content", "")
    if content is None:
        content = ""
    elif not isinstance(content, str):
        content = str(content)
    user_amount = sanitize(inputs.get("user_amount", ""))
    user_date = sanitize(inputs.get("user_date", ""))
    user_vendor = sanitize(inputs.get("user_vendor", ""))
    user_invoice_flag = sanitize(inputs.get("user_invoice_flag", ""))

    preprocessed = preprocess(
        content=content,
        user_amount=user_amount,
        user_date=user_date,
        user_vendor=user_vendor,
        user_invoice_flag=user_invoice_flag,
    )

    branch_tasks = {
        "amount_result": asyncio.create_task(
            asyncio.to_thread(amount_check, preprocessed["ocr_amount"], preprocessed["user_amount"]),
            name="amount_check",
        ),
        "date_result": asyncio.create_task(
            asyncio.to_thread(date_check, preprocessed["ocr_date"], preprocessed["user_date"]),
            name="date_check",
        ),
        "vendor_result": asyncio.create_task(
            asyncio.to_thread(vendor_check, preprocessed["ocr_vendor"], preprocessed["user_vendor"]),
            name="vendor_check",
        ),
        "invoice_result": asyncio.create_task(
            asyncio.to_thread(invoice_check, preprocessed["ocr_invoice_no"], preprocessed["user_invoice_flag"]),
            name="invoice_check",
        ),
    }

    branch_results = await asyncio.gather(*branch_tasks.values(), return_exceptions=True)
    resolved_results: dict[str, str] = {}
    errors: list[str] = []

    for key, result in zip(branch_tasks.keys(), branch_results):
        if isinstance(result, Exception):
            errors.append(f"{key}: {result}")
        else:
            resolved_results[key] = result

    if errors:
        raise WorkflowError("Parallel branch execution failed: " + "; ".join(errors))

    return {
        "amount_result": resolved_results["amount_result"],
        "date_result": resolved_results["date_result"],
        "vendor_result": resolved_results["vendor_result"],
        "invoice_result": resolved_results["invoice_result"],
        "ocr_invoice_flag": preprocessed["ocr_invoice_flag"],
    }


def run_workflow(inputs: dict[str, Any]) -> dict[str, str]:
    return asyncio.run(run_workflow_async(inputs))


def load_inputs(args: argparse.Namespace) -> dict[str, Any]:
    if args.input_json:
        return json.loads(args.input_json)
    if args.input_file:
        with open(args.input_file, "r", encoding="utf-8") as fh:
            return json.load(fh)

    if not sys.stdin.isatty():
        stdin_data = sys.stdin.read().strip()
        if stdin_data:
            return json.loads(stdin_data)

    content = args.content
    if args.content_file:
        with open(args.content_file, "r", encoding="utf-8") as fh:
            content = fh.read()

    return {
        "content": content or "",
        "user_amount": args.user_amount or "",
        "user_date": args.user_date or "",
        "user_vendor": args.user_vendor or "",
        "user_invoice_flag": args.user_invoice_flag or "",
    }


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run the Python port of the Dify workflow defined in ao.yml.",
    )
    parser.add_argument("--input-json", help="JSON object containing the workflow inputs.")
    parser.add_argument("--input-file", help="Path to a JSON file containing the workflow inputs.")
    parser.add_argument("--content", help="Raw OCR JSON string for the `content` input.")
    parser.add_argument("--content-file", help="Path to a file containing the OCR JSON string.")
    parser.add_argument("--user-amount", help="ユーザーが申請した金額")
    parser.add_argument("--user-date", help="ユーザーが申請した取引日")
    parser.add_argument("--user-vendor", help="ユーザーが申請した支払先名")
    parser.add_argument("--user-invoice-flag", help="ユーザーが申請した事業者Noの有無")
    return parser


def main() -> int:
    parser = build_arg_parser()
    args = parser.parse_args()

    try:
        inputs = load_inputs(args)
        result = run_workflow(inputs)
    except json.JSONDecodeError as exc:
        print(f"Invalid JSON input: {exc}", file=sys.stderr)
        return 2
    except WorkflowError as exc:
        print(str(exc), file=sys.stderr)
        return 1

    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
