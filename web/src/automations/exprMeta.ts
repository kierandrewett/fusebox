// Metadata for the expression language's built-in functions, mirroring the
// Rust evaluator in src/automations/expr.rs. Drives autocomplete + the
// reference shown in the expression editor.

export interface ExprFunction {
  name: string;
  /** Display signature, e.g. "jsonDecode(text)". */
  signature: string;
  doc: string;
}

export const EXPR_FUNCTIONS: ExprFunction[] = [
  { name: "jsonDecode", signature: "jsonDecode(text)", doc: "Parse a JSON string into a value/object/array." },
  { name: "jsonEncode", signature: "jsonEncode(value)", doc: "Serialise a value to a JSON string." },
  { name: "upper", signature: "upper(text)", doc: "Uppercase a string." },
  { name: "lower", signature: "lower(text)", doc: "Lowercase a string." },
  { name: "trim", signature: "trim(text)", doc: "Strip leading/trailing whitespace." },
  { name: "len", signature: "len(value)", doc: "Length of a string, array, or object." },
  { name: "contains", signature: "contains(haystack, needle)", doc: "Whether a string/array/object contains a value." },
  { name: "indexOf", signature: "indexOf(text, needle)", doc: "Index of needle in text, or -1." },
  { name: "replace", signature: "replace(text, from, to)", doc: "Replace all occurrences of a substring." },
  { name: "split", signature: "split(text, sep)", doc: "Split a string into an array." },
  { name: "join", signature: "join(array, sep)", doc: "Join array elements into a string." },
  { name: "substr", signature: "substr(text, start, end?)", doc: "Substring by character index." },
  { name: "startsWith", signature: "startsWith(text, prefix)", doc: "Whether text begins with prefix." },
  { name: "endsWith", signature: "endsWith(text, suffix)", doc: "Whether text ends with suffix." },
  { name: "capitalize", signature: "capitalize(text)", doc: "Uppercase the first character." },
  { name: "repeat", signature: "repeat(text, n)", doc: "Repeat text n times." },
  { name: "padStart", signature: 'padStart(text, len, pad?)', doc: "Left-pad text to len (pad defaults to a space)." },
  { name: "abs", signature: "abs(n)", doc: "Absolute value." },
  { name: "round", signature: "round(n, places?)", doc: "Round to nearest integer, or to N decimal places." },
  { name: "floor", signature: "floor(n)", doc: "Round down." },
  { name: "ceil", signature: "ceil(n)", doc: "Round up." },
  { name: "trunc", signature: "trunc(n)", doc: "Integer part." },
  { name: "sqrt", signature: "sqrt(n)", doc: "Square root." },
  { name: "pow", signature: "pow(base, exp)", doc: "base to the power of exp." },
  { name: "min", signature: "min(a, b, …) / min(array)", doc: "Smallest of the arguments, or of an array." },
  { name: "max", signature: "max(a, b, …) / max(array)", doc: "Largest of the arguments, or of an array." },
  { name: "clamp", signature: "clamp(n, lo, hi)", doc: "Constrain n to the range [lo, hi]." },
  { name: "random", signature: "random()", doc: "Pseudo-random number in [0, 1)." },
  { name: "urlEncode", signature: "urlEncode(text)", doc: "Percent-encode for use in a URL." },
  { name: "base64Encode", signature: "base64Encode(text)", doc: "Base64-encode text." },
  { name: "number", signature: "number(value)", doc: "Coerce to a number (null if not numeric)." },
  { name: "text", signature: "text(value)", doc: "Coerce to a string." },
  { name: "bool", signature: "bool(value)", doc: "Coerce to true/false (truthiness)." },
  { name: "type", signature: "type(value)", doc: "Type name: null/boolean/number/text/array/dictionary." },
  { name: "coalesce", signature: "coalesce(a, b, …)", doc: "First non-null argument." },
  { name: "now", signature: "now()", doc: "Current time in epoch milliseconds." },
  { name: "between", signature: 'between("07:30", "01:00")', doc: "True while the local time is in the window (wraps past midnight)." },
  { name: "year", signature: "year(ms?)", doc: "Local year, e.g. 2026. Optional epoch-ms argument; defaults to now." },
  { name: "month", signature: "month(ms?)", doc: "Local month number, 1-12." },
  { name: "day", signature: "day(ms?)", doc: "Local day of the month, 1-31." },
  { name: "hour", signature: "hour(ms?)", doc: "Local hour, 0-23." },
  { name: "minute", signature: "minute(ms?)", doc: "Local minute, 0-59." },
  { name: "second", signature: "second(ms?)", doc: "Local second, 0-59." },
  { name: "weekday", signature: "weekday(ms?)", doc: "Day of week as a number: 0 = Sunday … 6 = Saturday." },
  { name: "weekdayName", signature: "weekdayName(ms?)", doc: 'Day of week name, e.g. "Monday".' },
  { name: "monthName", signature: "monthName(ms?)", doc: 'Month name, e.g. "May".' },
  { name: "isWeekend", signature: "isWeekend(ms?)", doc: "True on Saturday or Sunday." },
  { name: "date", signature: "date(ms?)", doc: 'Local date as "YYYY-MM-DD".' },
  { name: "time", signature: "time(ms?)", doc: 'Local time of day as "HH:MM".' },
  { name: "deviceOn", signature: "deviceOn(name)", doc: "True if the named device is currently on." },
  { name: "deviceOff", signature: "deviceOff(name)", doc: "True if the named device is currently off." },
  { name: "deviceState", signature: "deviceState(name)", doc: "Device state: \"on\" / \"off\" / \"unknown\"." },
  { name: "sum", signature: "sum(array)", doc: "Sum of the numbers in an array." },
  { name: "avg", signature: "avg(array)", doc: "Average of the numbers in an array (null if empty)." },
  { name: "first", signature: "first(array)", doc: "First element, or null." },
  { name: "last", signature: "last(array)", doc: "Last element, or null." },
  { name: "sort", signature: "sort(array)", doc: "Sorted copy (numeric when all numbers, else text)." },
  { name: "reverse", signature: "reverse(array)", doc: "Reversed copy of an array." },
  { name: "slice", signature: "slice(array, start, end?)", doc: "Sub-array by index." },
  { name: "keys", signature: "keys(object)", doc: "Array of an object's keys." },
  { name: "values", signature: "values(object)", doc: "Array of an object's values." },
  { name: "entries", signature: "entries(object)", doc: "Array of [key, value] pairs." },
  { name: "merge", signature: "merge(a, b)", doc: "Merge two dictionaries (b wins on conflicts)." },
  { name: "get", signature: "get(value, key, default?)", doc: "Safe index into a dict/array, with optional fallback." },
];

export const EXPR_KEYWORDS = ["true", "false", "null", "input"] as const;
