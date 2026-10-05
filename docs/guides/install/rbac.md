# Restricting access

**RBAC decides what Axiom can see, and nothing else needs configuring.**
Discovery asks the API server which kinds the gateway's ServiceAccount may
list, and offers exactly those. To change what appears in SQL, change the
ClusterRoles.

That is the bound worth having, because the API server enforces it: a kind you
have not granted cannot be read, whatever a query asks for.

!!! warning "One identity for every query"

    Until per-caller identity lands, anyone who can query a foreign table reads
    as the gateway. Grant the gateway only what every user of that database may
    see, and use Postgres grants to narrow it further per role.
    [Giving an AI agent access](../agent-access.md) shows a scoped role.

## What the shipped role grants

`gateway-rbac.yaml` **reads broadly and writes narrowly.**

- **Reads**: everything in Kubernetes' `view` role (workloads, ConfigMaps,
  Services, NetworkPolicies, Ingresses, Events), plus nodes, storage, CRDs, RBAC
  objects, `events.k8s.io` and the metrics APIs.
- **Never Secrets.** No rule in the shipped role reaches them.
- **Writes**: granted per resource in the `axiom-gateway` ClusterRole: ConfigMaps
  and the example CRD.

## Narrowing it

Reads come from `axiom-gateway-read`, an aggregated ClusterRole that collects
two sets of roles:

- every role labelled `rbac.authorization.k8s.io/aggregate-to-view: "true"`,
  which is Kubernetes' own `view` role and any operator's;
- every role labelled `axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"`,
  which is the shipped `axiom-gateway-read-extra` (nodes, storage, CRDs, RBAC
  objects, metrics) and any you add.

To grant less, apply your own copy of `gateway-rbac.yaml`: drop the
`aggregate-to-view` selector from `axiom-gateway-read`, and trim
`axiom-gateway-read-extra` to the kinds you want. A kind granted only `get`,
`list` and `watch` is readable and cacheable but not writable, and its table
says so.

## Adding a custom resource

A custom resource is readable if its operator ships an `aggregate-to-view`
role. If it does not, label a read-only ClusterRole and the gateway's read role
picks it up:

```sh
kubectl apply -f - <<'YAML'
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: axiom-read-cnpg
  labels:
    axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"
rules:
  - apiGroups: ["postgresql.cnpg.io"]
    resources: ["*"]
    verbs: ["get", "list", "watch"]
YAML
```

**Name the API group** rather than using `"*"`: a rule that reaches the core
group with a wildcard grants Secrets.

## Making a kind writable

Add its verbs to the `axiom-gateway` ClusterRole, for example:

```sh
kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
  {"apiGroups":["apps"],"resources":["deployments"],"verbs":["get","list","watch","update"]}}]'
```

## After a change

- **A new grant** is seen on the next import, with no restart.
- **Revoking or narrowing one** needs a gateway restart, because the gateway
  caches what it is allowed.
- **Either way, import again.** Foreign tables are catalog objects and do not
  follow an RBAC change on their own. [Initialize](../initialize.md#a-kind-is-missing)
  shows how to check what the gateway offers now.
