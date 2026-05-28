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
  { name: "abs", signature: "abs(n)", doc: "Absolute value." },
  { name: "round", signature: "round(n)", doc: "Round to nearest integer." },
  { name: "floor", signature: "floor(n)", doc: "Round down." },
  { name: "ceil", signature: "ceil(n)", doc: "Round up." },
  { name: "trunc", signature: "trunc(n)", doc: "Integer part." },
  { name: "sqrt", signature: "sqrt(n)", doc: "Square root." },
  { name: "pow", signature: "pow(base, exp)", doc: "base to the power of exp." },
  { name: "min", signature: "min(a, b, …)", doc: "Smallest argument." },
  { name: "max", signature: "max(a, b, …)", doc: "Largest argument." },
  { name: "random", signature: "random()", doc: "Pseudo-random number in [0, 1)." },
  { name: "urlEncode", signature: "urlEncode(text)", doc: "Percent-encode for use in a URL." },
  { name: "base64Encode", signature: "base64Encode(text)", doc: "Base64-encode text." },
  { name: "number", signature: "number(value)", doc: "Coerce to a number (null if not numeric)." },
  { name: "text", signature: "text(value)", doc: "Coerce to a string." },
  { name: "bool", signature: "bool(value)", doc: "Coerce to true/false (truthiness)." },
  { name: "type", signature: "type(value)", doc: "Type name: null/boolean/number/text/array/dictionary." },
  { name: "coalesce", signature: "coalesce(a, b, …)", doc: "First non-null argument." },
  { name: "now", signature: "now()", doc: "Current time in epoch milliseconds." },
  { name: "deviceOn", signature: "deviceOn(name)", doc: "True if the named device is currently on." },
  { name: "deviceOff", signature: "deviceOff(name)", doc: "True if the named device is currently off." },
  { name: "deviceState", signature: "deviceState(name)", doc: "Device state: \"on\" / \"off\" / \"unknown\"." },
  { name: "keys", signature: "keys(object)", doc: "Array of an object's keys." },
  { name: "values", signature: "values(object)", doc: "Array of an object's values." },
];

export const EXPR_KEYWORDS = ["true", "false", "null", "input"] as const;
