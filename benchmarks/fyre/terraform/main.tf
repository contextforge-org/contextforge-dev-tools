data "fyre_user" "current" {}

data "fyre_quota" "current" {
  site = var.site
}

locals {
  account_default_product_group_id = try(
    data.fyre_user.current.development.default_product_group_id == null
    ? null
    : tostring(data.fyre_user.current.development.default_product_group_id),
    null,
  )
  sole_product_group_id = try(
    length(data.fyre_user.current.development.product_groups) == 1
    ? tostring(data.fyre_user.current.development.product_groups[0].id)
    : null,
    null,
  )
  product_group_id = (
    var.product_group_id != null ? var.product_group_id :
    local.account_default_product_group_id != null ? local.account_default_product_group_id :
    local.sole_product_group_id
  )
  quick_burn_available = contains(
    ["true", "yes", "y"],
    lower(try(data.fyre_user.current.development.quick_burn, "no")),
  )
  common = {
    os               = var.os
    platform         = "x"
    public_network   = "y"
    quota_type       = local.product_group_id == null ? "quick_burn" : "product_group"
    product_group_id = local.product_group_id
    site             = var.site
    ssh_keys         = [var.ssh_public_key]
  }
}

resource "fyre_vm" "locust" {
  hostname         = "cf-${var.run_id}-locust"
  description      = "ContextForge benchmark locust"
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

  lifecycle {
    precondition {
      condition     = local.product_group_id != null || local.quick_burn_available
      error_message = "FYRE account has no default or sole product group and no quick-burn quota; set FYRE_PRODUCT_GROUP_ID."
    }
  }
}

resource "fyre_vm" "fast_time" {
  hostname         = "cf-${var.run_id}-fast-time"
  description      = "ContextForge benchmark fast time"
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

  lifecycle {
    precondition {
      condition     = local.product_group_id != null || local.quick_burn_available
      error_message = "FYRE account has no default or sole product group and no quick-burn quota; set FYRE_PRODUCT_GROUP_ID."
    }
  }
}

resource "fyre_vm" "dataplane" {
  count            = var.dataplane_count
  hostname         = "cf-${var.run_id}-dataplane-${count.index + 1}"
  description      = "ContextForge benchmark dataplane replica ${count.index + 1}"
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

  lifecycle {
    precondition {
      condition     = local.product_group_id != null || local.quick_burn_available
      error_message = "FYRE account has no default or sole product group and no quick-burn quota; set FYRE_PRODUCT_GROUP_ID."
    }
  }
}
