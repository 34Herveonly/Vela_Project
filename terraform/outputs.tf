# Vela Terraform Outputs
# These values are printed after terraform apply completes
# Copy the server_ip into your GitHub secrets as GCP_SERVER_IP

output "server_ip" {
  description = "Public IP address of the Vela production server"
  value       = google_compute_address.vela_ip.address
}

output "server_name" {
  description = "Name of the GCP VM instance"
  value       = google_compute_instance.vela_server.name
}

output "ssh_command" {
  description = "Command to SSH into the server"
  value       = "ssh -i ~/.ssh/id_ed25519_gcp devops_admin@${google_compute_address.vela_ip.address}"
}

output "vela_health_url" {
  description = "URL to check Vela health once deployed"
  value       = "http://${google_compute_address.vela_ip.address}:7700/health"
}
