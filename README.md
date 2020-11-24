# Prototype code to automatically set up instance storage for CoreOS

This is WIP for https://hackmd.io/dTUvY7BIQIu_vFK5bMzYvg

## Quickstart w/OpenShift 4 (for workers)

The only supported platform at the moment is AWS.  More coming.

### Create a MachineConfig to set this up:

`oc create -f 50-worker-coreos-cloud-instance-store-provisioner.yaml`

Wait for the worker MCP to roll out to the latest (`oc get machineconfigpool/worker`).
Note: Existing machines will be reconfigured but do not actually change state in practice.

### Edit the machineset to use a m5d type that has instance storage

Next, `oc -n openshift-machine-api edit machineset/$x` and change the instance type use e.g. `m5d.xlarge` (150GiB instance store).

Scale up the machineset:

`oc -n openshift-machine-api scale machineset/$x`

When the new node joins the cluster, use e.g. `oc debug node/$node` and inspect `findmnt /var/lib/containers` - it should be a bind mount.


## Configuring the control plane

You can also configure the control plane to use this; there are
two variants of that - one for just the control plane's `/var/lib/containers`,
and one including that and `/var/lib/etcd`.  Doing both is by far
the most interesting; if you don't have a schedulable control
plane then just doing `/var/lib/containers` won't change too much.

You must configure the control plane "day 0" by providing
[additional manifests to the installer](https://github.com/openshift/installer/blob/master/docs/user/customization.md#install-time-customization-for-machine-configuration).

There is a `50-master-coreos-cloud-instance-store-provisioner.yaml`
you can use that is set up to do both directories.  Get ready
for much improved etcd performance!
