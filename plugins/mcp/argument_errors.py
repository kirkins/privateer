"""Value-free validation diagnostics. Never use ValidationError.message/repr."""
import re
import sys
sys.dont_write_bytecode = True


def field_name(name):
    # Unknown property names are untrusted too: bound them and prevent log injection.
    return name if isinstance(name, str) and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", name) else "arguments"


def diagnostic(error):
    rule = error.validator
    field = field_name(next(iter(error.absolute_path), "arguments"))
    if rule == "additionalProperties":
        extras = sorted(set(error.instance) - set(error.schema.get("properties", {})))
        field = field_name(extras[0]) if extras else "arguments"
        text = f"unknown field {field}; remove it (additionalProperties)"
    elif rule == "required":
        missing = [key for key in error.schema["required"] if key not in error.instance]
        field = field_name(missing[0])
        text = f"{field}: required field missing"
    elif rule in {"maxLength", "minLength"}:
        bound = error.schema[rule]  # schema constants, never the instance
        text = f"{field}: {'longer' if rule == 'maxLength' else 'shorter'} than {bound} characters"
    elif rule == "type":
        text = f"{field}: must be {error.schema['type']}"
    elif rule in {"minimum", "maximum", "minItems", "maxItems"}:
        text = f"{field}: violates {rule} {error.schema[rule]}"
    else:
        # Static keyword only; enum/pattern error messages include instance values.
        rule = rule if rule in {"enum", "pattern", "uniqueItems"} else "schema"
        text = f"{field}: violates {rule} rule"
    return field, rule, text
