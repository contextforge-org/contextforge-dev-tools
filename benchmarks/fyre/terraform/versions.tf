terraform {
  required_version = ">= 1.8, < 2.0"
  required_providers {
    fyre = {
      source  = "hashicorp-forge/fyre"
      version = "= 0.0.3"
    }
  }
}

provider "fyre" {}
