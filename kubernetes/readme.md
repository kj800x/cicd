CICD can't completely self-bootstrap it's own deployment yet, since it can't deploy namespaceless resources yet.

The following resources need to be applied manually by the sysadmin:
- ClusterRoleBinding
- ServiceAccount
- CRDs
- Namespace

Also need to manually create the ghcr-kj800x secret by hand (using invocation stored in notes)
