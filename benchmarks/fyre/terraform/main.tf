locals {
  common = {
    os               = var.os
    platform         = "x"
    public_network   = "y"
    quota_type       = var.product_group_id == null ? "quick_burn" : "product_group"
    product_group_id = var.product_group_id
    site             = var.site
    ssh_keys         = [var.ssh_public_key]
  }
}

resource "fyre_vm" "locust" {
  hostname         = "cf-${var.run_id}-locust"
  description      = "cf-integration FYRE benchmark ${var.run_id}; role=locust"
  os               = local.common.os
  platform         = local.common.platform
  public_network   = local.common.public_network
  quota_type       = local.common.quota_type
  product_group_id = local.common.product_group_id
  site             = local.common.site
  ssh_keys         = local.common.ssh_keys
  time_to_live     = local.common.quota_type == "quick_burn" ? tostring(var.expiry_hours) : null
  expiration       = local.common.quota_type == "product_group" ? "${var.expiry_hours} hours" : null
  cpu              = var.locust_cpu
  memory           = var.locust_memory_gb
  disable_delete   = "n"
}

resource "fyre_vm" "fast_time" {
  hostname         = "cf-${var.run_id}-fast-time"
  description      = "cf-integration FYRE benchmark ${var.run_id}; role=fast-time"
  os               = local.common.os
  platform         = local.common.platform
  public_network   = local.common.public_network
  quota_type       = local.common.quota_type
  product_group_id = local.common.product_group_id
  site             = local.common.site
  ssh_keys         = local.common.ssh_keys
  time_to_live     = local.common.quota_type == "quick_burn" ? tostring(var.expiry_hours) : null
  expiration       = local.common.quota_type == "product_group" ? "${var.expiry_hours} hours" : null
  cpu              = var.fast_time_cpu
  memory           = var.fast_time_memory_gb
  disable_delete   = "n"
}

resource "fyre_vm" "dataplane" {
  count            = var.dataplane_count
  hostname         = "cf-${var.run_id}-dataplane-${count.index + 1}"
  description      = "cf-integration FYRE benchmark ${var.run_id}; role=dataplane; replica=${count.index + 1}"
  os               = local.common.os
  platform         = local.common.platform
  public_network   = local.common.public_network
  quota_type       = local.common.quota_type
  product_group_id = local.common.product_group_id
  site             = local.common.site
  ssh_keys         = local.common.ssh_keys
  time_to_live     = local.common.quota_type == "quick_burn" ? tostring(var.expiry_hours) : null
  expiration       = local.common.quota_type == "product_group" ? "${var.expiry_hours} hours" : null
  cpu              = var.dataplane_cpu
  memory           = var.dataplane_memory_gb
  disable_delete   = "n"
}
