variable "name" {
  description = "Server name and prefix for resource names and network tags."
  type        = string
  default     = "tnt"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,25}[a-z0-9]$", var.name))
    error_message = "Use 3–27 lowercase letters, digits, or hyphens, starting with a letter and ending with a letter or digit (compatible with the service account suffix)."
  }
}

variable "ssh_public_key" {
  description = "OpenSSH public key authorized for the wumpa user (without a username prefix)."
  type        = string
}

variable "project_name" {
  description = "Display name for the GCP project."
  type        = string
}

variable "nixos_image_archive" {
  description = "Local x86_64 NixOS GCE .raw.tar.gz archive; relative paths are resolved from tnt/terraform when using -chdir."
  type        = string

  validation {
    condition     = fileexists(var.nixos_image_archive) && endswith(var.nixos_image_archive, ".raw.tar.gz")
    error_message = "Build/export the NixOS .raw.tar.gz archive locally before planning."
  }
}

variable "project_id" {
  description = "Globally unique GCP project ID. Also update the bucket in state.tf."
  type        = string
}

variable "region" {
  description = "Region for the subnet, storage buckets, and image."
  type        = string
  default     = "europe-west1"
}

variable "zone" {
  description = "VM zone; must belong to region."
  type        = string
  default     = "europe-west1-b"
}

variable "machine_type" {
  description = "VM size for the Wumpa server and remote development workloads."
  type        = string
  default     = "e2-standard-2"
}

variable "boot_disk_size_gb" {
  description = "Boot disk size; also holds checkouts and Wumpa configuration."
  type        = number
  default     = 50
}
