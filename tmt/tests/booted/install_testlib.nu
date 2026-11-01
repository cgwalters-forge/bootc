use tap.nu

# Build a derived image, optionally regenerating its UKI as the repart tests do.
export def build [image: string, lines: string, --uki] {
    let containerfile = $"FROM localhost/bootc as base\n($lines)\n"
    let containerfile = if $uki { tap make_uki_containerfile $containerfile } else { $containerfile }
    # Keep an OCI manifest, see https://github.com/bootc-dev/bootc/issues/1703
    $containerfile | podman build --format oci -t $image . -f -
}

# Copy the booted image once before building or installing from localhost/bootc.
export def copy_to_storage [] {
    bootc image copy-to-storage
    "containers-storage:localhost/bootc"
}

export def create_disk [disk: string, size: string = "10G"] {
    truncate -s $size $disk
}

# The optional runner lets a test retain tap run_install's mount namespace.
export def install_to_disk [
    image: string
    disk: string
    args: list<string> = []
    --env-vars: list<string> = []
    --volumes: list<string> = []
    --runner: closure
] {
    let env_args = $env_vars | each {|e| [--env $e] } | flatten
    let volume_args = $volumes | each {|v| [-v $v] } | flatten
    let cmd = [podman run --rm --privileged --pid=host
        --security-opt label=type:unconfined_t
        -v /dev:/dev -v /var/lib/containers:/var/lib/containers -v /var/tmp:/var/tmp]
        | append $env_args | append $volume_args
        | append [$image bootc install to-disk --via-loopback]
        | append $args | append $disk
    if $runner == null { run-external ...$cmd } else { do $runner $cmd }
}

# Detach even if partition discovery or the inspection closure fails.
export def with_disk [disk: string, inspect: closure] {
    let loop = losetup -f --show $disk | str trim
    let result = try {
        partx -u $loop
        udevadm settle
        {value: (do $inspect $loop)}
    } catch {|e| {error: $e} }
    losetup -d $loop
    if $result.error? != null { error make $result.error }
    $result.value
}

# Mount only the requested partition (root or ESP), always read-only.
export def with_mount [disk: string, mountpoint: string, inspect: closure, --partition: string = "root"] {
    with_disk $disk {|loop|
        let parts = sfdisk --json $loop | from json | get partitiontable.partitions
        let part = if $partition == "root" {
            $parts | where name == "root" | first
        } else if $partition == "esp" {
            $parts | where {|p| ($p.type | str downcase) == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" } | first
        } else {
            error make {msg: $"Unknown partition: ($partition)"}
        }
        mkdir $mountpoint
        mount -o ro $part.node $mountpoint
        let result = try { {value: (do $inspect $mountpoint)} } catch {|e| {error: $e} }
        umount $mountpoint
        if $result.error? != null { error make $result.error }
        $result.value
    }
}
