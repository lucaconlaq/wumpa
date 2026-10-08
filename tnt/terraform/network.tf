resource "google_compute_network" "main" {
  name                    = "${var.name}-vpc"
  auto_create_subnetworks = false

  depends_on = [google_project_service.main["compute.googleapis.com"]]
}

resource "google_compute_subnetwork" "main" {
  name          = "${var.name}-subnet"
  network       = google_compute_network.main.id
  region        = var.region
  ip_cidr_range = "10.10.0.0/24"
}

# Google's IAP TCP forwarding range. No public SSH or Wumpa ingress.
resource "google_compute_firewall" "ssh" {
  name    = "${var.name}-allow-iap-ssh"
  network = google_compute_network.main.id

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }

  source_ranges = ["35.235.240.0/20"]
  target_tags   = [var.name]
}
