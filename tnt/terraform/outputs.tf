output "nixos_image" {
  description = "Compute Engine image registered from the local archive."
  value       = google_compute_image.nixos.id
}

output "project_id" {
  value = google_project.main.project_id
}

output "external_ip" {
  description = "Outbound IP; direct inbound SSH is not allowed."
  value       = google_compute_instance.main.network_interface[0].access_config[0].nat_ip
}

output "ssh_proxy_command" {
  description = "ProxyCommand for the server alias in ~/.ssh/config."
  value       = "gcloud compute start-iap-tunnel ${google_compute_instance.main.name} 22 --listen-on-stdin --project=${google_project.main.project_id} --zone=${google_compute_instance.main.zone} --verbosity=warning"
}

output "ssh_command" {
  description = "Requires the SSH configuration documented in ../README.md."
  value       = "ssh ${var.name}"
}
