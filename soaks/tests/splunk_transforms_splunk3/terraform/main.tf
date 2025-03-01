terraform {
  required_providers {
    kubernetes = {
      version = "~> 2.5.0"
      source  = "hashicorp/kubernetes"
    }
  }
}

provider "kubernetes" {
  config_path = "~/.kube/config"
}

module "monitoring" {
  source       = "../../../common/terraform/modules/monitoring"
  type         = var.type
  vector_image = var.vector_image
}

resource "kubernetes_namespace" "soak" {
  metadata {
    name = "soak"
  }
}

module "vector" {
  source       = "../../../common/terraform/modules/vector"
  type         = var.type
  vector_image = var.vector_image
  vector-toml  = file("${path.module}/vector.toml")
  namespace    = kubernetes_namespace.soak.metadata[0].name
  vector_cpus  = var.vector_cpus
  depends_on   = [module.monitoring, module.http-blackhole]
}
module "http-blackhole" {
  source              = "../../../common/terraform/modules/lading_http_blackhole"
  type                = var.type
  http-blackhole-yaml = file("${path.module}/../../../common/configs/http_blackhole.yaml")
  namespace           = kubernetes_namespace.soak.metadata[0].name
  lading_image        = var.lading_image
  depends_on          = [module.monitoring]
}
module "http-gen" {
  source        = "../../../common/terraform/modules/lading_http_gen"
  type          = var.type
  http-gen-yaml = file("${path.module}/../../../common/configs/http_gen_splunk_source.yaml")
  namespace     = kubernetes_namespace.soak.metadata[0].name
  lading_image  = var.lading_image
  depends_on    = [module.monitoring, module.vector]
}
