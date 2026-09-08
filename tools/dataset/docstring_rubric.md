# Docstring Examples

Real docstrings pulled from this codebase, sorted into three buckets.

A **good** docstring helps the person calling the code, not the person who wrote it: parameters, return contract, gotchas, and when to reach for it. It stays true even after the implementation is rewritten, because it describes a contract rather than a body. It never carries a ticket ID, PR number, phase label, or a story about how the code used to work.

A **bad** docstring is bloated relative to what it documents, narrates the implementation line-by-line instead of explaining usage, reads like a spec written before the code, or drags in point-in-time history that will go stale the moment the ticket it cites is closed.

A docstring can also fail by leaking a cross-reference: explaining how some OTHER module, caller, or sibling function uses, depends on, mirrors, or must stay in sync with this code, instead of stating this code's own contract. "Matches ObjectHelper._has_access_to_note", "same approach as in foo()", "called from the billing job", and "keep in sync with constants in other_file.go" are all the same failure: naming who or what else is involved instead of restating the hazard in this code's own terms. A precondition on any caller, stated generically ("call this before mutating X", "never pass caller input") is not this failure — it is a self-contained contract; naming a specific other file, class, or caller as the reason is.

Sometimes the right docstring is **no docstring** — a name, a signature, and a one-line body already say everything a caller needs, and adding prose on top would only be noise.

---

## Good examples

### `rollout` — the two permissions-v2 feature flags
Module docstring explaining a cross-flag dependency that lives nowhere else.

```python
"""The two per-tenant flags that stage the permissions v2 rollout.

Reading either flag any other way loses the dependency between them, so
treat this module as the only entry point:

- ``authz_enforcement`` decides whether authorization outcomes come from the
  v2 engine or from the legacy role predicate. It is temporary; it is
  removed once every tenant runs enforced.
- ``authz_tenant_managed`` lets a tenant's admins customize roles, role
  assignments, and the seat-license-to-role association. It is permanent, and
  it requires ``authz_enforcement`` — customizing authorization that decides
  nothing would mislead the admin making the change.

A lookup that fails resolves to the safe answer, ``False``: audit mode keeps
the legacy predicate deciding, and tenant-managed off keeps customization
closed. A tenant with no ``features`` row reads as disabled for the same
reason.
"""
```
`myapp/api/authz/rollout.py`

Tells a consumer what each flag means, why one requires the other, and what happens on a failed lookup — all facts that survive any rewrite of the flag-reading code underneath, stated without naming the sibling helper class this module wraps. No ticket references, no rollout timeline, just the contract.

### `DbRole` — one role, a named list of permissions
Class docstring stating invariants that don't live in any single column.

```python
class DbRole(DbBaseModel, DbStandardModel):
    """One role: a named list of permissions.

    A role with no owner tenant is builtin: it ships with the product and is
    read-only for customers. A role with an owner tenant is custom to that
    tenant. Among live rows, builtin names are unique globally and custom
    names are unique within their tenant; soft-deleted rows release their
    name. Two invariants live at the service layer because they cross rows or
    tables: a custom name must not collide with any builtin name, and a
    non-restricted role must not contain restricted permissions.

    A ``restricted`` role is assignable and visible only in the platform
    tenant.
    """
```
`myapp/api/authz/models/db_role.py`

Says which rules a caller cannot see just from the column list — the uniqueness scope, what soft-delete does to a name, and which invariants are enforced above the model rather than by the schema. Zero mention of how or when this was built.

### `_verify_reactivation_seat_gate` — a call-order hazard
Function docstring warning about SQLAlchemy autoflush before the caller can find out the hard way.

```python
def _verify_reactivation_seat_gate(user: User, desired_role_code: str | None) -> str | None:
    """Seat/license check for a role change and/or a deactivate->activate transition.

    Call this BEFORE mutating ``user.is_active`` or ``user.user_role`` on the
    session-attached ``user``: the seat/role counts filter on ``User.is_active``,
    and SQLAlchemy's autoflush would otherwise flush the pending state change and
    count the user being reactivated as already active -- double-counting the
    very seat it is trying to occupy and blocking the last legitimate seat.

    Client-portal users and anyone who would never occupy a contracted
    seat in the first place (``occupies_contracted_seat_role`` -- a seat-exempt
    role, or an internal @example.com address) are never gated. Returns an error
    message when the gate blocks the transition, None when clear.
    """
```
`myapp/api/base/routes.py`

A caller who mutates `user.is_active` before calling this hits a subtle self-inflicted bug that only shows up as an incorrect seat count; the docstring surfaces that hazard directly and states the return contract in the same breath.

### `in_clause` — a rendering gotcha in four lines
Short function, right-sized docstring.

```python
def in_clause(column: str, values) -> str:
    """Render a SQL ``IN`` predicate pinning a column to a fixed vocabulary.

    Values are inlined rather than bound because this feeds ``CheckConstraint``,
    which is DDL — a bound parameter would not survive into the schema. Callers
    pass vocabularies defined in code, never caller input.
    """
    return column + " IN (" + ", ".join(f"'{value}'" for value in values) + ")"
```
`myapp/api/base/constraints.py`

Two sentences carry the two facts a caller actually needs: why this inlines instead of binding, and the one safety rule (never feed it user input) that the function itself does nothing to enforce.

### `email_action_token_service` — why this is a service, not a query helper
Module docstring stating the two disciplines that justify its existence.

```python
"""Minting and consumption of email action tokens.

The service is identity-free: every scoping value arrives as an explicit
argument, so the same code serves a send path running under an org user and an
inbound path running under nobody.

Two disciplines are the reason this is a service rather than a handful of
queries.

**Consume before acting.** ``consume`` marks the token spent and commits before
returning, so the caller performs its action against an already-spent token. A
crash in between loses the action, never the single-use guarantee -- the
fail-safe direction.

**Re-check every bound field.** A caller states the purpose and tenant it
believes it is serving, and any single deviation collapses to one generic
negative outcome. Trusting the row's own scoping is what silently loses the
cross-tenant check.
"""
```
`myapp/api/email_actions/services/email_action_token_service.py`

Explains the ordering guarantee (commit-before-act) and the security discipline (never trust the row's own scoping) a caller must preserve — both true regardless of how the minting internals change.

### `FeatureHelper.is_feature_enabled_by_code` — precedence and two gotchas
Method docstring covering what the return type can't.

```python
@classmethod
def is_feature_enabled_by_code(cls, code: str, tenant_id: int) -> bool:
    """One flag, tenant override winning over the global value.

    Resolves through the select path rather than ``Feature.query`` so callers
    outside a flask_sqlalchemy-bound app read it too, and an absent flag row
    reads as off instead of raising.
    """
    enabled = DB().scalar(
        select(func.coalesce(TenantFeature.feature_enabled, Feature.feature_enabled))
        .select_from(outerjoin(Feature, TenantFeature, and_(Feature.id == TenantFeature.feature_id, TenantFeature.tenant_id == tenant_id)))
        .where(Feature.feature_code == code)
    )
    return bool(enabled)
```
`myapp/api/features/helpers.py`

States the precedence rule (tenant beats global) plus two behaviors a caller needs and can't infer from the signature: it works outside a Flask app context, and a missing row means "off," not an exception.

### `BaseObjectHandler.check_access` — the mandatory entry point
Method docstring distinguishing itself from an ordinary access check.

```python
def check_access(self, user_id: int, tenant_id: int, object_id: int) -> bool:
    """Entry point callers must use to gate note access to an object.

    Loads the user once and enforces the notes-wide policy that customer
    users are always denied -- unconditionally, since notes carry no per-row
    customer-visibility field to check -- before deferring to the handler's
    own role-specific check. Centralized here so a new handler can't land
    without it.

    Args:
        user_id: The user ID to check access for
        tenant_id: The tenant ID for scoping
        object_id: The database integer ID of the object

    Returns:
        True if user has access, False otherwise
    """
```
`myapp/api/notes/handlers/base_object_handler.py`

Tells the consumer the one thing the name alone doesn't: this is the required gate, not an optional check, and states the security policy it enforces — unconditional denial, because there is no per-row field to consult — before any subclass runs. It stays accurate regardless of how individual handlers implement their own check, and it no longer needs another class's name to make the point.

### `DbTemplateRule.series_uuid` — a fallback that isn't a bug
Property docstring explaining a deliberate default.

```python
@property
def series_uuid(self) -> str:
    """The identity to key cross-document references on.

    Falls back to `uuid` rather than being backfilled: a rule that has never been
    cloned IS the first version of its own lineage, so its own uuid is the right
    answer and stays the answer once `clone_rules_for_template` stamps it onto the
    copy. A NULL backing value means "not inherited", not "unknown" -- never
    treat it as missing data that needs a backfill.
    """
    return self.series_rule_uuid or str(self.uuid)
```
`myapp/api/template_automation/models/db_template_rule.py`

Explains why the fallback is correct rather than a stopgap, and states a convention (`NULL` means "not inherited") in this property's own terms, not by pointing the reader at other columns in the table to go verify.

### `role_used_by_workspace_items` — counting items without double-counting
Method docstring stating a self-contained counting hazard.

```python
@classmethod
def role_used_by_workspace_items(cls, workspace_id: int, role_id: int, tenant_id: int) -> bool:
    """Whether any item on this workspace currently has this role.

    Counts items by ``DbItem.workspace_id`` directly rather than walking
    the ``DbItemMapping``->``DbSectionMapping`` mapping chain, which has
    no ``.distinct()`` and can double-count: an item reachable through
    more than one mapping path must still be counted once, not per path.
    """
```
`myapp/api/workspace/helperClasses/workspace_role_derivation_service.py`

States the counting rule in this function's own terms -- which column it counts on, and the exact bug (a non-distinct join chain) a naive rewrite would reintroduce -- without naming the function it must agree with or the route that calls it. Both were true facts, but neither is part of this function's own contract: a caller who breaks the "count once per item" invariant breaks it whether or not they've ever heard of role_item_counts.

### `utc_naive_now` — when to use it, and why the alternative breaks
Function docstring for a genuinely subtle timezone gotcha.

```python
def utc_naive_now() -> datetime:
    """Now in UTC, as a naive datetime — the unambiguous form for a naive column.

    `DateTime` (no `timezone=True`) is `timestamp without time zone`. psycopg2 adapts
    an AWARE datetime with an explicit `::timestamptz` cast, and Postgres then applies
    the `timestamptz -> timestamp` assignment cast using the SESSION's `TimeZone`.
    ...
    Stripping tzinfo after converting to UTC makes the stored value independent of the
    driver and the session TimeZone. Use this for every naive column — including as the
    column `default`, where a bare `datetime.now` would otherwise store process-local
    wall-clock; use get_current_timestamp_with_timezone() for `DateTime(timezone=True)`.
    """
    return datetime.now(timezone.utc).replace(tzinfo=None)
```
`myapp/api/utils/dates.py`

Earns its length: the bug it prevents (a stored timestamp that silently depends on the DB session's timezone setting) is real and non-obvious, and the docstring tells you exactly when to reach for this versus its aware-datetime sibling.

---

## Bad examples

### `normalize_workspace_slug` — a docstring that only talks about other files
Function docstring naming every caller and sibling instead of stating its own contract.

```python
def normalize_workspace_slug(raw: str) -> str:
    """Normalize a workspace slug for storage and lookup.

    Called from WorkspaceCreateService.create() and from the PATCH
    /workspaces/{id}/rename route, both of which run this before writing
    DbWorkspace.slug. The reporting ETL's workspace_dim loader does the same
    normalization independently in etl/dims/workspace_dim.py, so any change
    here must be mirrored there or the warehouse and the app will disagree on
    which two slugs are "the same" workspace.

    See WorkspaceCreateService.create for the full validation this feeds into.
    """
    return raw.strip().lower().replace(" ", "-")
```
`myapp/api/workspace/slugs.py`

Every sentence names some OTHER piece of code -- two callers, an ETL loader it must be kept in sync with, and a pointer to a different docstring for "the real" contract -- and none of it is a fact about what normalize_workspace_slug itself does. Delete the call graph and one sentence survives that a caller actually needs: it strips, lowercases, and dashes. If the ETL agreement is a real hazard, it belongs restated in this function's own terms ("must match the transform applied wherever slugs are compared for equality") or as a comment on the other file -- never as a fact this docstring has to carry about a file two directories away.

### `legacy_providers` — a maintenance note wearing a docstring
Module docstring that talks to future engineers, not callers.

```python
"""Legacy activity table providers for the /activity endpoint.

TEMPORARY: This module contains ALL legacy activity logic -- queries, cursors,
merge-sort, and filter helpers for deprecated tables (workspace_activity,
template_activity).

DELETE THIS FILE when legacy data migration is complete (see PROJ-4112).
Deleting it will break imports in helpers.py -- follow those references to clean up.
"""
```
`myapp/api/activity_history/legacy_providers.py`

This is a to-do list for whoever retires the module, complete with a ticket ID, not documentation of what it does for a consumer. A better version would say what queries the module serves today and let the tracker carry the deprecation plan.

### `DbStandardModel.is_archived` — restating the return line, then cutting off
Two twin docstrings that add nothing and one that doesn't even finish its sentence.

```python
@hybrid_property
def is_archived(self) -> bool:
    """
    Python property to check if the object has been archived.
    """
    return self.archived_at is not None and self.archived_by is not None

@is_archived.expression
def is_archived(cls) -> bool:
    """
    SQL expression to check if the object has been
    """
    return cls.archived_at.is_not(None) & cls.archived_by.is_not(None)
```
`myapp/api/base/db_standard_model.py`

Each docstring just re-says the property name in a sentence, and the SQL-expression twin never even finishes the thought ("...check if the object has been" — has been what?). Deleting both loses zero information; the property name and one-line body already say it.

### `EventDispatchService.on_note_created` — the docstring hides the interesting part
One-line docstring that names the method and stops there.

```python
def on_note_created(
    self,
    user_id: int,
    tenant_id: int,
    note_id: int,
    object_type_code: str,
    object_id: int,
    object_name: str,
    event_type_code: str,
) -> None:
    """Dispatch note created event for notes on any object type"""
    try:
        if not self.pipeline.has_webhooks_for_event(event_type_code):
            return
        payload = self.note_event_payload_helper.get_note_created_event_dto_by_id(...)
        self.pipeline.on_event(event_type=event_type_code, event_data=payload.to_dict())
    except Exception as exc:
        logger.warn(f"Failed to publish {event_type_code} event", exc_info=exc)
```
`myapp/api/event/services/event_dispatch_service.py`

"Dispatch note created event" is just the method name in prose — it tells a caller nothing about the actual contract, which is that a dispatch failure is caught and logged rather than raised. That swallowed-exception behavior is exactly what a docstring here should surface.

### `integrations_pipeline` web layer — a 94-line design essay
Module docstring that argues architecture instead of describing usage.

```python
"""The executor's adapter-facing web layer.

A bare ``FastAPI()`` here is deliberate, not an oversight the
``service_lambda_auth``/``create_secured_app`` gate should catch: this
Lambda's only caller is the cloud provider itself — the web adapter forwarding
a queue event-source-mapping batch to ``POST /events``, or its own readiness
probe against ``GET /health``. There is no untrusted external caller to gate
with a JWT middleware here (unlike the CRM broker, which is reachable by other
authenticated services), so this file never imports ``service_lambda_auth``
and is not a "consumer" the repo-wide bare-FastAPI CI scan tracks.

Transport authentication for ``POST /events`` -- why an ARN pin, not a
header: this Lambda has no Function URL, so nothing ever reaches it as a
real inbound HTTP request. The queue invokes the function directly...
"""
# ... docstring continues verbatim for 79 lines total, going on to argue queue
# partial-batch-failure semantics, envelope version-skew handling, and
# redelivery-timing rationale.
```
`myapp/api/integrations_pipeline/web.py`

Ninety-four lines total, none of which tell a caller how to use anything — it argues why the file is shaped the way it is (a CI-scan exemption, an IAM boundary, a three-way queue failure taxonomy). That argument needs rewriting every time the reasoning changes, which is exactly what a docstring should never require.

### `RuleExecutionTracker.log_rule_execution` — a spec that admits its own decay
Docstring written like a pre-code design doc, ending mid-shrug.

```python
def log_rule_execution(self, rule: DbQueueRule, action: Dict, context: Dict) -> None:
    """
    Log the execution of a rule action to the queue_activity table

    :rule The DbQueueRule being executed
    :action The specific action being performed (dict from action_configuration or trigger_configuration)
    :context Dict containing execution context
            - user_id: ID of the user triggering the action
            - inbound_id: Optional ID of the inbound item (if applicable)
            - queue_id: ID of the queue
            - tenant_id: ID of the stage
            - error: optional error dict
            ... more as we expand
    """
```
`myapp/api/queues/models/db_queue_activity.py`

Enumerating today's `context` keys field-by-field violates open/closed outright, and "... more as we expand" is an admission that the list will go stale the next time a key is added. A better version states what the log record is for and leaves the dict's shape to the type hints.

### `edge_gateway` — an incident report living in the module header
Module docstring that narrates its own past mistake instead of documenting the gate.

```python
"""Origin-authorization gate for deployments fronted by a CDN edge.

Where this instance's Function URL is reachable from the internet, this gate is
what authorizes the caller: the CDN's edge-dispatch function injects a
shared secret header on every origin request, and nothing else knows the value.
A request without it never reaches a route.

... [several more paragraphs on request-signing vs. shared-secret rationale and a fail-closed truth table] ...

The third case is NOT what makes the rollout safe -- do not reason that way. An
earlier version of this docstring claimed a redeployed edge in front of a
not-yet-redeployed process "passes through untouched", and that belief took five
live previews down. It is wrong twice over: the edge decides per ORIGIN, not
globally, and at a legacy row it does not inject this header at all -- it
signs the request, so a Lambda that predates the secret keeps behaving exactly
as before.
"""
```
`myapp/api/middleware/edge_gateway.py`

At 49 lines this is a postmortem, literally citing what "an earlier version of this docstring" got wrong and what it cost. That belongs in an incident writeup or the git history, not in front of every future reader trying to find out how the gate works today.

### `workspace_time_controller` — a product-rationale essay above two routes
Module docstring justifying business decisions instead of documenting the endpoints below it.

```python
"""The time figures a customer is ever shown.

Two integers for one workspace: hours worked and hours allotted. Everything this
controller withholds is deliberate and structural rather than conditional --
no per-person hours, which expose the staffing mix and invite "why did a
junior do this?"; no variance framing; no note text, written for internal
readers...

[continues for 44 lines total: business rationale for every design decision,
gating rules, which settings toggle interacts with which document, why a
sibling email setting is deliberately NOT consulted here]
"""
```
`myapp/api/client_portal/controllers/workspace_time_controller.py`

Forty-four lines of product rationale — why per-person hours are hidden, why one setting doesn't apply here — buried instead of stated as a short, scannable contract. A consumer of this controller needs the actual invariants (which two fields are returned, which settings gate them), not the case for the product decision.

### `process_api_workspace_payload` — the request schema re-typed as prose
Docstring that duplicates the wire contract and dates itself with a ticket ID.

```python
@classmethod
def process_api_workspace_payload(cls, workspace_json: dict, tenant_id: int, user):
    """
    Process Public API `/workspaces/create_from_api` payload and convert to
    CreateWorkspaceInputDTO. `user` is the calling API principal, used for
    field-level edit-permission validation on `datafields`. Expected payload keys:
    {
        "name": str,  # REQUIRED
        "start_date": str,  # REQUIRED (MM/DD/YYYY)
        "workspace_owner": str,  # REQUIRED — UUID or email
        ...
        # PROJ-4810 expansion — all forwarded from public-api. Omitted keys defer
        # to the column server_default (booleans) or the template value (for fields
        # the template config-apply step substitutes). `description` and
        # `workspace_value` are `nullable_when_set=True` on the public-api contract —
        # explicit `null` clears the field even when a template is supplied:
        "description": str | None,  # OPTIONAL — template substitution on omit; explicit null clears
        ...
    }
    Returns (CreateWorkspaceInputDTO, ApiResponse | None).
    """
```
`myapp/api/workspace/services.py`

Forty-two lines re-typing an entire payload schema as a dict literal, with a ticket ID sitting in the middle of it. Nothing enforces that this stays in sync with the real DTO as fields are added, and the DTO's own type definitions are the one place this contract can't drift.

### `audit_stamp_for` — a headcount that will go stale
A useful first sentence, buried under a census of today's callers.

```python
def audit_stamp_for(obj, column_name: str) -> datetime:
    """`now()` in the form `obj.<column_name>` can store without a lossy cast.

    This decides the form for EVERY created_at / modified_at stamp the application makes
    through DB.add_transaction — roughly 28 naive declarations across 12 modules
    (data_fields, file, integrations/esign, platform_integrations, tags, todos, queues,
    drops, webhooks, workspace/incoming, workspace_automation, item) plus every
    DbStandardModel-inherited aware one. DbItem.modified_at is the override the item
    lifecycle cares about, but it is not the only column affected.

    So it asks the mapped column which kind it is rather than assuming aware — see
    utc_naive_now() for why an aware value in a naive column is session-TimeZone-dependent.
    ...
    """
```
`myapp/api/utils/dates.py`

The opening sentence is exactly the contract a caller needs; everything after it — an exact count of today's naive columns and a named list of the 12 modules that have them — will be wrong the moment a 13th module adds one. A count of current callers is not part of this function's contract.

### `UserSession.invalidate` — the docstring as a translation of the name
Three-line method, three lines of pure restatement.

```python
def invalidate(self, reason: str = "logout"):
    """Mark the session as invalid."""
    self.is_active = False
    self.invalidated_dts = datetime.now(timezone.utc)
    self.invalidation_reason = reason
```
`myapp/api/user/models.py`

"Mark the session as invalid" says nothing the method name didn't already say, and skips the one thing a caller might actually want to know — that a reason gets recorded and a timestamp gets stamped. Deleting it costs nothing; a docstring naming what fields get set would cost one more sentence and add real value.

---

## Where no docstring is the right call

### `domain_label` — a lookup-or-default in one line
```python
def domain_label(domain: str) -> str:
    return DOMAIN_LABEL_OVERRIDES.get(domain, domain.replace("-", " ").title())
```
`myapp/api/authz/dtos/permission_dtos.py`

The whole function is one expression: look up an override, else title-case the input. A docstring would only restate the line directly below it.

### `Account.ui_object_link` — a property that builds one URL
```python
@property
def ui_object_link(self):
    return f"/dashboard/accounts/{self.account_uuid}?tab=details"
```
`myapp/api/account/models.py`

Name plus a single f-string already tell the whole story. Padding this with a docstring adds nothing a reader can't see in the same glance.

### `UserDTO` — a flat data carrier
```python
@dataclass_json
@dataclass
class UserDTO:
    id: int
    first_name: str
    last_name: str
    email_address: str
    profile_pic: str
```
`myapp/api/comments/dtos/comment_user_dto.py`

The field list is the entire contract. Any class-level docstring here would just repeat the class name in prose.

### `DbCustomEmailTemplate.__repr__` — a dunder with an obvious job
```python
def __repr__(self):
    return f"<DbCustomEmailTemplate(id={self.id}, name='{self.name}', template_type='{self.template_type}')>"
```
`myapp/api/custom_email_templates/models/db_custom_email_template.py`

Every Python developer already knows what `__repr__` is for, and the return statement is the whole implementation. Nothing to add.

### `FeatureHelper.get_feature_by_code_query` — a query builder that is its own description
```python
@classmethod
def get_feature_by_code_query(cls, code):
    return select(Feature).where(Feature.feature_code == code)
```
`myapp/api/features/helpers.py`

Name and body say the identical thing: build a select-by-code query. Compare this to its neighbor `is_feature_enabled_by_code` above, which earns a docstring because it hides a real precedence rule and two behavioral gotchas — this one hides nothing.

### `RuleExecutionTracker.__init__` — a constructor that stores its argument
```python
class RuleExecutionTracker:
    def __init__(self, db_session: DB):
        self.db_session = db_session
```
`myapp/api/queues/models/db_queue_activity.py`

One argument, assigned to a same-named attribute — the signature is the whole story. Notice the contrast with `log_rule_execution` in the same class, which over-documents a method by turning its `context` dict into a spec that goes stale; here, less is exactly right.

### `ClientFavoriteController.GET_favorites` — a route whose flow is its own explanation
```python
@staticmethod
def GET_favorites(workspace_id):
    try:
        workspace_id = decode_id(workspace_id)
        if workspace_id is None:
            return ApiResponse().badRequest("Invalid workspace id.")
        tenant, user = _authorized_caller(request, workspace_id)
        if tenant is None:
            return ApiResponse().unauthorized("Not signed in.")
        favorites = ClientFavoriteService.list_for_user(workspace_id, user.id, tenant.id)
        return ApiResponse().success(data={"favorites": [...], "cap": FAVORITE_CAP})
    except ...
```
`myapp/api/client_favorite/controllers/client_favorite_controller.py`

The name plus the linear decode-authorize-fetch-serialize flow already tell a reader everything about this route. A docstring would only restate the HTTP verb and noun the name already spells out.

### `_is_form` — a predicate that names its own check
```python
def _is_form(question: QuestionRef) -> bool:
    return question.element_uuid is not None
```
`myapp/api/workspace_answer_import/services/matching.py`

The function name states the question; the body states the answer. A docstring would just narrate "returns whether element_uuid is not None" back in words.

### `WorkspaceCrud.delete_all` / `simple_query` — pass-through overrides
```python
def delete_all(self, ids: list, commit: bool = True):
    return super().delete_all(ids, commit)

def simple_query(self, class_metadata: DefaultMeta, filters: Optional[dict] = None, order_by=None) -> Query:
    return super().simple_query(class_metadata, filters, order_by)
```
`myapp/api/workspace/crud.py`

Both methods forward to the base class with an identical signature and no added behavior. Any documentation belongs on the base class itself — repeating it here would drift out of sync with zero benefit.
