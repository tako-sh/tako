# Portable HTTP schema validation

Service contracts accept inline TypeBox strings, finite numbers, integers, booleans,
nulls, literals, arrays, objects, unions and intersections. The keyword allowlist in
`contract.ts` is the supported subset; references, transforms, formats and other
JSON Schema keywords are rejected. TypeBox handles structure. Small interpreted
checks handle scalar constraints and JSON array uniqueness without runtime string
code generation.

- String lengths count Unicode code points. Patterns use JavaScript Unicode regular
  expressions (`u` flag).
- Numbers use JavaScript's finite number domain. `multipleOf` uses exact integer
  arithmetic on the normalized decimal spellings of the value and positive divisor.
  This accepts `0.3` as a multiple of `0.1` without accepting tiny nonzero multiples
  of one. It does not recover precision already lost by JSON parsing or JavaScript
  arithmetic: `0.1 + 0.2` has the spelling `0.30000000000000004` and is not a multiple
  of `0.1` under this rule.
- Object `required` metadata must name exactly the declared properties without
  OptionalKind markers, with no duplicates. Overrides that contradict static types
  are rejected at definition time; generation reconstructs the same requirements.
- `uniqueItems` uses pairwise JSON structural equality, including `-0 === 0` and
  order-independent object properties. It does not trust hashes. Its worst-case
  cost is quadratic in array length, multiplied by item comparison cost. Contracts
  can bound array length with `maxItems`; host request-size limits remain host-owned.
- Validation returns an own-property snapshot. Outgoing bodies serialize that
  exact snapshot once, without invoking caller `toJSON` hooks or rereading getters.
  Object snapshots have null prototypes. Array snapshots copy own indices, ignore
  custom iterators, and shadow inherited `toJSON`. Shared object references are
  copied once; cycles are rejected. JSON bodies reject non-finite numbers and
  non-JSON own values, including undeclared additional properties. Getters execute
  once when copied and must be ordinary application code, not a sandbox boundary.

Snapshotting uses memory proportional to the payload. Incoming JSON objects exposed
to handlers and clients retain null prototypes; use `Object.hasOwn` for membership.
Regular expression execution and payload depth/size limits remain the host's concern.
