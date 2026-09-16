variable "run_id" {
  type = string
  validation {
    condition     = can(regex("^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$", var.run_id))
    error_message = "run_id must be a lowercase DNS-safe identifier of at most 48 characters."
  }
}

variable "os" { type = string }
variable "ssh_public_key" { type = string }
variable "expiry_hours" { type = number }
variable "dataplane_count" { type = number }
variable "dataplane_cpu" { type = number }
variable "dataplane_memory_gb" { type = number }
variable "locust_cpu" { type = number }
variable "locust_memory_gb" { type = number }
variable "fast_time_cpu" { type = number }
variable "fast_time_memory_gb" { type = number }

variable "product_group_id" {
  type      = string
  default   = null
  nullable  = true
  sensitive = true
}

variable "site" {
  type     = string
  default  = null
  nullable = true
}
