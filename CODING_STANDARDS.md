# Coding standards

Read during review. Each rule is a judgement call; mechanical rules live in CI
(`.github/workflows/tests.yml`) and `.githooks/`.

## Fix the cause, everywhere it occurs

A bug fix names the root cause and removes it at every site that shares it, not only the one in
the report. Search for siblings before approving.

Example: PR #21 stopped dashboard pills from wrapping. The cause was class names built from data
("Empty" → `.empty`) colliding with the page's own classes; PR #23 fixed that a day later.

## Owner complaints get the general capability

When a change answers an owner complaint about the agent ("it couldn't move my workout"), check
that the agent can now handle the whole kind of request, not just the example in the complaint.
A new narrow verb per complaint is a smell. The owner complained about refusals on 2026-10-05,
2026-10-09 and 2026-10-12; each fix added verbs, and the next request still fell through.
