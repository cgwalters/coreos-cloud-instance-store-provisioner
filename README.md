# Prototype code to automatically set up instance storage for CoreOS

This is WIP for https://hackmd.io/dTUvY7BIQIu_vFK5bMzYvg

## Quickstart w/OpenShift 4

The only supported platform at the moment is AWS.  More coming.

### Create a MachineConfig to set this up:

`oc create -f 43-coreos-cloud-instance-store-provisioner.yaml`

Wait for the worker MCP to roll out to the latest (`oc get machineconfigpool/worker`).
Note: Existing machines will be reconfigured but do not actually change state in practice.

### Edit the machineset to use a m5d type that has instance storage

Next, `oc -n openshift-machine-api edit machineset/$x` and change the instance type use e.g. `m5d.xlarge` (150GiB instance store).

Scale up the machineset:

`oc -n openshift-machine-api scale machineset/$x`

When the new node joins the cluster, use e.g. `oc debug node/$node` and inspect `findmnt /var/lib/containers` - it should be a bind mount.



