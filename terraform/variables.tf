# Vela Terraform Variables

variable "project_id" {
  description = "GCP Project ID"
  type        = string
  default     = "vela-project-507218"
}

variable "region" {
  description = "GCP region for all resources"
  type        = string
  default     = "africa-south1"
}

variable "zone" {
  description = "GCP zone for the VM"
  type        = string
  default     = "africa-south1-b"
}

variable "ssh_public_key" {
  description = "SSH public key for server access"
  type        = string
  # This is your id_ed25519_gcp.pub content
  default     = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILjak3i2QBmzjMRsiwH3GvstVta0HggGRXarCUWhNyki vela-gcp-server"
}

variable "machine_type" {
  description = "GCP VM machine type"
  type        = string
  default     = "e2-small"
}
