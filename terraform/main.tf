# Vela Production Infrastructure
# Provider: Google Cloud Platform
# Region: africa-south1 (Johannesburg)
# Authentication: Application Default Credentials (gcloud auth)

terraform {
  required_version = ">= 1.0"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
  }
}

# Tell Terraform which GCP project and region to use
provider "google" {
  project = var.project_id
  region  = var.region
  zone    = var.zone
}

# Static external IP address
# This gives Vela a permanent IP that never changes
# Even if the VM is stopped and restarted
resource "google_compute_address" "vela_ip" {
  name   = "vela-static-ip"
  region = var.region
}

# Firewall rule — allow SSH from anywhere
# Required for Ansible and manual server access
resource "google_compute_firewall" "allow_ssh" {
  name    = "vela-allow-ssh"
  network = "default"

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }

  # Restrict to your IP in production
  # For now allow all for ease of setup
  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["vela-server"]
}

# Firewall rule — allow Vela API port
# Port 7700 is the Vela REST API
resource "google_compute_firewall" "allow_vela_api" {
  name    = "vela-allow-api"
  network = "default"

  allow {
    protocol = "tcp"
    ports    = ["7700"]
  }

  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["vela-server"]
}

# Firewall rule — allow HTTP for health checks
# Port 80 for future nginx or load balancer
resource "google_compute_firewall" "allow_http" {
  name    = "vela-allow-http"
  network = "default"

  allow {
    protocol = "tcp"
    ports    = ["80", "443"]
  }

  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["vela-server"]
}

# The Vela production VM
resource "google_compute_instance" "vela_server" {
  name         = "vela-production"
  machine_type = "e2-small"
  zone         = var.zone

  # Apply the firewall rules via tags
  tags = ["vela-server"]

  # Boot disk — Ubuntu 22.04 LTS
  boot_disk {
    initialize_params {
      image = "ubuntu-os-cloud/ubuntu-2204-lts"
      size  = 20  # GB
      type  = "pd-standard"
    }
  }

  # Network configuration
  network_interface {
    network = "default"

    # Attach the static IP we reserved above
    access_config {
      nat_ip = google_compute_address.vela_ip.address
    }
  }

  # SSH key for Ansible and manual access
  # Format: "username:ssh-public-key"
  metadata = {
    ssh-keys = "devops_admin:${var.ssh_public_key}"
  }

  # Startup script — runs once when VM first boots
  # Installs minimal dependencies before Ansible takes over
  metadata_startup_script = <<-EOF
    #!/bin/bash
    apt-get update -y
    apt-get install -y curl wget git
    echo "Vela production server ready for Ansible configuration" > /tmp/ready.txt
  EOF

  # Allow the VM to call GCP APIs if needed
  service_account {
    scopes = ["cloud-platform"]
  }
}
