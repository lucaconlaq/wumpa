resource "google_storage_bucket" "terraform" {
  project                     = google_project.main.project_id
  name                        = "${var.project_id}-terraform"
  location                    = var.region
  force_destroy               = false
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"

  versioning {
    enabled = true
  }

  lifecycle {
    prevent_destroy = true
  }

  depends_on = [google_project_service.main["storage.googleapis.com"]]
}
