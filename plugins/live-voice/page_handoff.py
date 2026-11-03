"""Narrow page-language exceptions to the delegated outcome-word guard.

Only a completed turn consisting of client_read_page calls may use these.
This does not confer authority: its answer still uses the screen-data frame.
Approval verbs and all sending words remain subject to the ordinary guard.
"""
import re


FLEET_CONTEXT = re.compile(r"\b(?:captain|deploy|deployment|fleet|ship|recipient|message)\b")
PAGE_APPROVAL_TOPIC = re.compile(
    r"\b(?:page|article|document) (?:discusses|describes|explains|covers|lists|is about) "
    r"(?:the )?approvals? (?:requirements?|process(?:es)?|polic(?:y|ies)|workflows?)\b")
ACCESS_GRANT = re.compile(
    r"(?:the )?(?:(?:page|browser|tab|site|document) "
    r"|(?:page|article|document) says "
    r"|(?:page|article|document) (?:discusses|describes|explains|covers|lists|is about) "
    r"(?:the )?approvals? (?:requirements?|process(?:es)?|polic(?:y|ies)|workflows?) and says )"
    r"(?:access|permissions?)"
    r"(?: (?:to|read|the|this|current|page|browser|tab|site|document|has|have|had|is|was|were|been|"
    r"not|yet|never|already|explicitly|previously|hasnt|havent|hadnt|isnt|wasnt|werent)){0,12}"
    r" (?P<grant>granted|authorized|authorised)"
    r"(?: (?:after|when|once|because) [a-z0-9 ]+)?")


def guard_words(normalized: str, *, completed_page_read: bool) -> set[str]:
    """Keep the existing guard, except explicit browser-access/topic phrases.

    The small phrase list deliberately fails closed on other wording. A page
    mentioning fleet outcomes never gets this exception, including when a
    harmless access statement and an outcome claim appear in the same answer.
    """
    if completed_page_read and not FLEET_CONTEXT.search(normalized):
        # Require a whole, browser-qualified statement. Remove only its grant
        # word so other outcome words in an explanation remain guarded. The
        # optional after/when/once/because tail must never be stripped: it can
        # contain a real approval or authorization claim of its own.
        grant = ACCESS_GRANT.fullmatch(normalized)
        if grant is not None:
            start, end = grant.span("grant")
            normalized = normalized[:start] + normalized[end:]
        normalized = PAGE_APPROVAL_TOPIC.sub("", normalized)
    return set(normalized.split())
