# Dedicated identity with no project roles: Wumpa does not need GCP API access.
resource "google_service_account" "main" {
  account_id   = "${var.name}-vm"
  display_name = "${var.name} Wumpa server"
  project      = google_project.main.project_id

  depends_on = [google_project_service.main["iam.googleapis.com"]]
}

resource "google_compute_instance" "main" {
  name         = var.name
  machine_type = var.machine_type
  zone         = var.zone
  tags         = [var.name]

  deletion_protection       = true
  allow_stopping_for_update = true

  boot_disk {
    initialize_params {
      image = google_compute_image.nixos.self_link
      size  = var.boot_disk_size_gb
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.main.id

    # Outbound internet access; inbound access is restricted by the firewall.
    access_config {}
  }

  service_account {
    email  = google_service_account.main.email
    scopes = ["cloud-platform"]
  }

  metadata = {
    enable-oslogin         = "FALSE"
    block-project-ssh-keys = "TRUE"
    ssh-keys               = "wumpa:${var.ssh_public_key}"
  }

  # Protect checkouts and Wumpa state from accidental VM replacement.
  lifecycle {
    prevent_destroy = true
  }

  depends_on = [
    google_project_service.main["iap.googleapis.com"],
  ]
}
