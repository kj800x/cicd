CICD can't completely self-bootstrap it's own deployment yet, since it can't deploy namespaceless resources yet.

The following resources need to be applied manually by the sysadmin:
- ClusterRoleBinding
- ServiceAccount
- CRDs
- Namespace

Also need to manually create the ghcr-kj800x secret by hand (using invocation stored in notes)

## Template Namespace Feature

The application supports automatic namespace initialization with resource copying via the `TEMPLATE_NAMESPACE` environment variable.

### How It Works

When deploying to a namespace that doesn't exist:
1. The namespace is automatically created
2. If `TEMPLATE_NAMESPACE` is configured, all resources from the template namespace are copied to the new namespace
3. Resources that already exist in the target namespace are skipped (not overwritten)
4. Application resources are then deployed normally

### Use Cases

- **Infrastructure Resources**: Copy ConfigMaps, Secrets, ServiceAccounts, NetworkPolicies, etc. that are required for all namespaces
- **Default Configuration**: Ensure new namespaces have standard RBAC, resource quotas, or limit ranges
- **Shared Dependencies**: Copy common dependencies like image pull secrets or service mesh configurations

### Configuration

Set the `TEMPLATE_NAMESPACE` environment variable to the name of the namespace containing the template resources:

```bash
export TEMPLATE_NAMESPACE=infrastructure
```

### Behavior

- **Resource Copying**: Only namespaced resources are copied (cluster-scoped resources are skipped)
- **Idempotent**: Copying is safe to run multiple times - existing resources are never overwritten
- **Error Handling**: Copy failures are logged but don't prevent namespace creation or deployment
- **Metadata Cleanup**: Owner references and namespace-specific metadata are removed during copying
