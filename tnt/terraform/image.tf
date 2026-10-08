locals {
  nixos_image_sha256 = filesha256(var.nixos_image_archive)
}

resource "google_storage_bucket" "images" {
  project                     = google_project.main.project_id
  name                        = "${var.project_id}-images"
  location                    = var.region
  force_destroy               = false
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"

  depends_on = [google_project_service.main["storage.googleapis.com"]]
}

# Content-addressed names ensure rebuilt archives produce a new GCE image.
resource "google_storage_bucket_object" "nixos" {
  name         = "nixos/${local.nixos_image_sha256}.raw.tar.gz"
  bucket       = google_storage_bucket.images.name
  source       = var.nixos_image_archive
  content_type = "application/gzip"
}

resource "google_compute_image" "nixos" {
  project           = google_project.main.project_id
  name              = "${var.name}-nixos-${substr(local.nixos_image_sha256, 0, 20)}"
  storage_locations = [var.region]

  raw_disk {
    source = "https://storage.googleapis.com/${google_storage_bucket_object.nixos.bucket}/${google_storage_bucket_object.nixos.name}"
  }

  lifecycle {
    create_before_destroy = true
  }

  depends_on = [google_project_service.main["compute.googleapis.com"]]
}
