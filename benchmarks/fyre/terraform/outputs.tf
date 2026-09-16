locals {
  locust_ips = {
    for address in fyre_vm.locust.ips : address.type => address.ip
  }
  fast_time_ips = {
    for address in fyre_vm.fast_time.ips : address.type => address.ip
  }
  dataplane_ips = [for vm in fyre_vm.dataplane : {
    name = vm.hostname
    id   = vm.vm_id
    ips  = { for address in vm.ips : address.type => address.ip }
  }]
}

output "inventory" {
  value = {
    run_id = var.run_id
    locust = {
      name       = fyre_vm.locust.hostname
      id         = fyre_vm.locust.vm_id
      public_ip  = try(local.locust_ips.public, "")
      private_ip = try(local.locust_ips.private, "")
    }
    fast_time = {
      name       = fyre_vm.fast_time.hostname
      id         = fyre_vm.fast_time.vm_id
      public_ip  = try(local.fast_time_ips.public, "")
      private_ip = try(local.fast_time_ips.private, "")
    }
    dataplanes = [for vm in local.dataplane_ips : {
      name       = vm.name
      id         = vm.id
      public_ip  = try(vm.ips.public, "")
      private_ip = try(vm.ips.private, "")
    }]
  }
}
