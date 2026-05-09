---
title: "awscc.cloudfront.Distribution"
description: "AWSCC CloudFront Distribution resource reference"
---


CloudFormation Type: `AWS::CloudFront::Distribution`

A distribution tells CloudFront where you want content to be delivered from, and the details about how to track and manage content delivery.

## Argument Reference

### `distribution_config`

- **Type:** [Struct(DistributionConfig)](#distributionconfig)
- **Required:** Yes

The distribution's configuration.

### `tags`

- **Type:** `Map<String, String>`
- **Required:** No

A complex type that contains zero or more ``Tag`` elements.

## Enum Values

### viewer_protocol_policy (ViewerProtocolPolicy)

| Value | DSL Identifier |
|-------|----------------|
| `allow-all` | `awscc.cloudfront.Distribution.ViewerProtocolPolicy.allow_all` |
| `redirect-to-https` | `awscc.cloudfront.Distribution.ViewerProtocolPolicy.redirect_to_https` |
| `https-only` | `awscc.cloudfront.Distribution.ViewerProtocolPolicy.https_only` |

Shorthand formats: `allow_all` or `ViewerProtocolPolicy.allow_all`

### ip_address_type (IpAddressType)

| Value | DSL Identifier |
|-------|----------------|
| `ipv4` | `awscc.cloudfront.Distribution.IpAddressType.ipv4` |
| `ipv6` | `awscc.cloudfront.Distribution.IpAddressType.ipv6` |
| `dualstack` | `awscc.cloudfront.Distribution.IpAddressType.dualstack` |

Shorthand formats: `ipv4` or `IpAddressType.ipv4`

### origin_protocol_policy (OriginProtocolPolicy)

| Value | DSL Identifier |
|-------|----------------|
| `http-only` | `awscc.cloudfront.Distribution.OriginProtocolPolicy.http_only` |
| `match-viewer` | `awscc.cloudfront.Distribution.OriginProtocolPolicy.match_viewer` |
| `https-only` | `awscc.cloudfront.Distribution.OriginProtocolPolicy.https_only` |

Shorthand formats: `http_only` or `OriginProtocolPolicy.http_only`

### connection_mode (ConnectionMode)

| Value | DSL Identifier |
|-------|----------------|
| `direct` | `awscc.cloudfront.Distribution.ConnectionMode.direct` |
| `tenant-only` | `awscc.cloudfront.Distribution.ConnectionMode.tenant_only` |

Shorthand formats: `direct` or `ConnectionMode.direct`

### event_type (FunctionAssociationEventType)

| Value | DSL Identifier |
|-------|----------------|
| `viewer-request` | `awscc.cloudfront.Distribution.FunctionAssociationEventType.viewer_request` |
| `viewer-response` | `awscc.cloudfront.Distribution.FunctionAssociationEventType.viewer_response` |
| `origin-request` | `awscc.cloudfront.Distribution.FunctionAssociationEventType.origin_request` |
| `origin-response` | `awscc.cloudfront.Distribution.FunctionAssociationEventType.origin_response` |

Shorthand formats: `viewer_request` or `FunctionAssociationEventType.viewer_request`

### restriction_type (RestrictionType)

| Value | DSL Identifier |
|-------|----------------|
| `none` | `awscc.cloudfront.Distribution.RestrictionType.none` |
| `blacklist` | `awscc.cloudfront.Distribution.RestrictionType.blacklist` |
| `whitelist` | `awscc.cloudfront.Distribution.RestrictionType.whitelist` |

Shorthand formats: `none` or `RestrictionType.none`

### event_type (LambdaFunctionAssociationEventType)

| Value | DSL Identifier |
|-------|----------------|
| `viewer-request` | `awscc.cloudfront.Distribution.LambdaFunctionAssociationEventType.viewer_request` |
| `origin-request` | `awscc.cloudfront.Distribution.LambdaFunctionAssociationEventType.origin_request` |
| `origin-response` | `awscc.cloudfront.Distribution.LambdaFunctionAssociationEventType.origin_response` |
| `viewer-response` | `awscc.cloudfront.Distribution.LambdaFunctionAssociationEventType.viewer_response` |

Shorthand formats: `viewer_request` or `LambdaFunctionAssociationEventType.viewer_request`

### selection_criteria (OriginGroupSelectionCriteria)

| Value | DSL Identifier |
|-------|----------------|
| `default` | `awscc.cloudfront.Distribution.OriginGroupSelectionCriteria.default` |
| `media-quality-based` | `awscc.cloudfront.Distribution.OriginGroupSelectionCriteria.media_quality_based` |

Shorthand formats: `default` or `OriginGroupSelectionCriteria.default`

### minimum_protocol_version (MinimumProtocolVersion)

| Value | DSL Identifier |
|-------|----------------|
| `sni-only` | `awscc.cloudfront.Distribution.MinimumProtocolVersion.sni_only` |
| `true` | `awscc.cloudfront.Distribution.MinimumProtocolVersion.true` |

Shorthand formats: `sni_only` or `MinimumProtocolVersion.sni_only`

### ssl_support_method (SslSupportMethod)

| Value | DSL Identifier |
|-------|----------------|
| `sni-only` | `awscc.cloudfront.Distribution.SslSupportMethod.sni_only` |
| `vip` | `awscc.cloudfront.Distribution.SslSupportMethod.vip` |
| `static-ip` | `awscc.cloudfront.Distribution.SslSupportMethod.static_ip` |

Shorthand formats: `sni_only` or `SslSupportMethod.sni_only`

### mode (ViewerMtlsMode)

| Value | DSL Identifier |
|-------|----------------|
| `required` | `awscc.cloudfront.Distribution.ViewerMtlsMode.required` |
| `optional` | `awscc.cloudfront.Distribution.ViewerMtlsMode.optional` |
| `passthrough` | `awscc.cloudfront.Distribution.ViewerMtlsMode.passthrough` |

Shorthand formats: `required` or `ViewerMtlsMode.required`

## Struct Definitions

### CacheBehavior

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `allowed_methods` | `List<String>` | No | A complex type that controls which HTTP methods CloudFront processes and forwards to your Amazon S3 bucket or your custom origin. There are three choices: + CloudFront forwards only ``GET`` and ``HEAD`` requests. + CloudFront forwards only ``GET``, ``HEAD``, and ``OPTIONS`` requests. + CloudFront forwards ``GET, HEAD, OPTIONS, PUT, PATCH, POST``, and ``DELETE`` requests. If you pick the third choice, you may need to restrict access to your Amazon S3 bucket or to your custom origin so users can't perform operations that you don't want them to. For example, you might not want users to have permissions to delete objects from your origin. |
| `cache_policy_id` | String | No | The unique identifier of the cache policy that is attached to this cache behavior. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. A ``CacheBehavior`` must include either a ``CachePolicyId`` or ``ForwardedValues``. We recommend that you use a ``CachePolicyId``. |
| `cached_methods` | `List<String>` | No | A complex type that controls whether CloudFront caches the response to requests using the specified HTTP methods. There are two choices: + CloudFront caches responses to ``GET`` and ``HEAD`` requests. + CloudFront caches responses to ``GET``, ``HEAD``, and ``OPTIONS`` requests. If you pick the second choice for your Amazon S3 Origin, you may need to forward Access-Control-Request-Method, Access-Control-Request-Headers, and Origin headers for the responses to be cached correctly. |
| `compress` | Bool | No | Whether you want CloudFront to automatically compress certain files for this cache behavior. If so, specify true; if not, specify false. For more information, see [Serving Compressed Files](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/ServingCompressedFiles.html) in the *Amazon CloudFront Developer Guide*. |
| `default_ttl` | Float | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. This field is deprecated. We recommend that you use the ``DefaultTTL`` field in a cache policy instead of this field. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. The default amount of time that you want objects to stay in CloudFront caches before CloudFront forwards another request to your origin to determine whether the object has been updated. The value that you specify applies only when your origin does not add HTTP headers such as ``Cache-Control max-age``, ``Cache-Control s-maxage``, and ``Expires`` to objects. For more information, see [Managing How Long Content Stays in an Edge Cache (Expiration)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. |
| `field_level_encryption_id` | String | No | The value of ``ID`` for the field-level encryption configuration that you want CloudFront to use for encrypting specific fields of data for this cache behavior. |
| `forwarded_values` | [Struct(ForwardedValues)](#forwardedvalues) | No | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. For more information, see [Working with policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/working-with-policies.html) in the *Amazon CloudFront Developer Guide*. If you want to include values in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. If you want to send values to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) or [Using the managed origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-origin-request-policies.html) in the *Amazon CloudFront Developer Guide*. A ``CacheBehavior`` must include either a ``CachePolicyId`` or ``ForwardedValues``. We recommend that you use a ``CachePolicyId``. A complex type that specifies how CloudFront handles query strings, cookies, and HTTP headers. |
| `function_associations` | [List\<FunctionAssociation\>](#functionassociation) | No | A list of CloudFront functions that are associated with this cache behavior. CloudFront functions must be published to the ``LIVE`` stage to associate them with a cache behavior. |
| `grpc_config` | [Struct(GrpcConfig)](#grpcconfig) | No | The gRPC configuration for your cache behavior. |
| `lambda_function_associations` | [List\<LambdaFunctionAssociation\>](#lambdafunctionassociation) | No | A complex type that contains zero or more Lambda@Edge function associations for a cache behavior. |
| `max_ttl` | Float | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. This field is deprecated. We recommend that you use the ``MaxTTL`` field in a cache policy instead of this field. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. The maximum amount of time that you want objects to stay in CloudFront caches before CloudFront forwards another request to your origin to determine whether the object has been updated. The value that you specify applies only when your origin adds HTTP headers such as ``Cache-Control max-age``, ``Cache-Control s-maxage``, and ``Expires`` to objects. For more information, see [Managing How Long Content Stays in an Edge Cache (Expiration)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. |
| `min_ttl` | Float | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. This field is deprecated. We recommend that you use the ``MinTTL`` field in a cache policy instead of this field. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. The minimum amount of time that you want objects to stay in CloudFront caches before CloudFront forwards another request to your origin to determine whether the object has been updated. For more information, see [Managing How Long Content Stays in an Edge Cache (Expiration)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. You must specify ``0`` for ``MinTTL`` if you configure CloudFront to forward all headers to your origin (under ``Headers``, if you specify ``1`` for ``Quantity`` and ``*`` for ``Name``). |
| `origin_request_policy_id` | String | No | The unique identifier of the origin request policy that is attached to this cache behavior. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) or [Using the managed origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-origin-request-policies.html) in the *Amazon CloudFront Developer Guide*. |
| `path_pattern` | String | Yes | The pattern (for example, ``images/*.jpg``) that specifies which requests to apply the behavior to. When CloudFront receives a viewer request, the requested path is compared with path patterns in the order in which cache behaviors are listed in the distribution. You can optionally include a slash (``/``) at the beginning of the path pattern. For example, ``/images/*.jpg``. CloudFront behavior is the same with or without the leading ``/``. The path pattern for the default cache behavior is ``*`` and cannot be changed. If the request for an object does not match the path pattern for any cache behaviors, CloudFront applies the behavior in the default cache behavior. For more information, see [Path Pattern](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#DownloadDistValuesPathPattern) in the *Amazon CloudFront Developer Guide*. |
| `realtime_log_config_arn` | Arn | No | The Amazon Resource Name (ARN) of the real-time log configuration that is attached to this cache behavior. For more information, see [Real-time logs](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/real-time-logs.html) in the *Amazon CloudFront Developer Guide*. |
| `response_headers_policy_id` | String | No | The identifier for a response headers policy. |
| `smooth_streaming` | Bool | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. Indicates whether you want to distribute media files in the Microsoft Smooth Streaming format using the origin that is associated with this cache behavior. If so, specify ``true``; if not, specify ``false``. If you specify ``true`` for ``SmoothStreaming``, you can still distribute other content using this cache behavior if the content matches the value of ``PathPattern``. |
| `target_origin_id` | String | Yes | The value of ``ID`` for the origin that you want CloudFront to route requests to when they match this cache behavior. |
| `trusted_key_groups` | `List<String>` | No | A list of key groups that CloudFront can use to validate signed URLs or signed cookies. When a cache behavior contains trusted key groups, CloudFront requires signed URLs or signed cookies for all requests that match the cache behavior. The URLs or cookies must be signed with a private key whose corresponding public key is in the key group. The signed URL or cookie contains information about which public key CloudFront should use to verify the signature. For more information, see [Serving private content](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/PrivateContent.html) in the *Amazon CloudFront Developer Guide*. |
| `trusted_signers` | `List<String>` | No | We recommend using ``TrustedKeyGroups`` instead of ``TrustedSigners``. This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. A list of AWS-account IDs whose public keys CloudFront can use to validate signed URLs or signed cookies. When a cache behavior contains trusted signers, CloudFront requires signed URLs or signed cookies for all requests that match the cache behavior. The URLs or cookies must be signed with the private key of a CloudFront key pair in the trusted signer's AWS-account. The signed URL or cookie contains information about which public key CloudFront should use to verify the signature. For more information, see [Serving private content](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/PrivateContent.html) in the *Amazon CloudFront Developer Guide*. |
| `viewer_protocol_policy` | [Enum (ViewerProtocolPolicy)](#viewer_protocol_policy-viewerprotocolpolicy) | Yes | The protocol that viewers can use to access the files in the origin specified by ``TargetOriginId`` when a request matches the path pattern in ``PathPattern``. You can specify the following options: + ``allow-all``: Viewers can use HTTP or HTTPS. + ``redirect-to-https``: If a viewer submits an HTTP request, CloudFront returns an HTTP status code of 301 (Moved Permanently) to the viewer along with the HTTPS URL. The viewer then resubmits the request using the new URL. + ``https-only``: If a viewer sends an HTTP request, CloudFront returns an HTTP status code of 403 (Forbidden). For more information about requiring the HTTPS protocol, see [Requiring HTTPS Between Viewers and CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-https-viewers-to-cloudfront.html) in the *Amazon CloudFront Developer Guide*. The only way to guarantee that viewers retrieve an object that was fetched from the origin using HTTPS is never to use any other protocol to fetch the object. If you have recently changed from HTTP to HTTPS, we recommend that you clear your objects' cache because cached objects are protocol agnostic. That means that an edge location will return an object from the cache regardless of whether the current request protocol matches the protocol used previously. For more information, see [Managing Cache Expiration](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. |

### CacheTagConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `header_name` | String | Yes | The name of the HTTP header that your origin includes in responses. CloudFront uses this header to extract cache tags. The header value must contain comma-separated tag values (for example, ``product:electronics, category:tv, brand:example``). |

### ConnectionFunctionAssociation

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | String | Yes | The association's ID. |

### Cookies

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `forward` | String | Yes | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. If you want to include cookies in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) in the *Amazon CloudFront Developer Guide*. If you want to send cookies to the origin but not include them in the cache key, use origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) in the *Amazon CloudFront Developer Guide*. Specifies which cookies to forward to the origin for this cache behavior: all, none, or the list of cookies specified in the ``WhitelistedNames`` complex type. Amazon S3 doesn't process cookies. When the cache behavior is forwarding requests to an Amazon S3 origin, specify none for the ``Forward`` element. |
| `whitelisted_names` | `List<String>` | No | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. If you want to include cookies in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) in the *Amazon CloudFront Developer Guide*. If you want to send cookies to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) in the *Amazon CloudFront Developer Guide*. Required if you specify ``whitelist`` for the value of ``Forward``. A complex type that specifies how many different cookies you want CloudFront to forward to the origin for this cache behavior and, if you want to forward selected cookies, the names of those cookies. If you specify ``all`` or ``none`` for the value of ``Forward``, omit ``WhitelistedNames``. If you change the value of ``Forward`` from ``whitelist`` to ``all`` or ``none`` and you don't delete the ``WhitelistedNames`` element and its child elements, CloudFront deletes them automatically. For the current limit on the number of cookie names that you can whitelist for each cache behavior, see [CloudFront Limits](https://docs.aws.amazon.com/general/latest/gr/xrefaws_service_limits.html#limits_cloudfront) in the *General Reference*. |

### CustomErrorResponse

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `error_caching_min_ttl` | Float | No | The minimum amount of time, in seconds, that you want CloudFront to cache the HTTP status code specified in ``ErrorCode``. When this time period has elapsed, CloudFront queries your origin to see whether the problem that caused the error has been resolved and the requested object is now available. For more information, see [Customizing Error Responses](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/custom-error-pages.html) in the *Amazon CloudFront Developer Guide*. |
| `error_code` | Int | Yes | The HTTP status code for which you want to specify a custom error page and/or a caching duration. |
| `response_code` | Int | No | The HTTP status code that you want CloudFront to return to the viewer along with the custom error page. There are a variety of reasons that you might want CloudFront to return a status code different from the status code that your origin returned to CloudFront, for example: + Some Internet devices (some firewalls and corporate proxies, for example) intercept HTTP 4xx and 5xx and prevent the response from being returned to the viewer. If you substitute ``200``, the response typically won't be intercepted. + If you don't care about distinguishing among different client errors or server errors, you can specify ``400`` or ``500`` as the ``ResponseCode`` for all 4xx or 5xx errors. + You might want to return a ``200`` status code (OK) and static website so your customers don't know that your website is down. If you specify a value for ``ResponseCode``, you must also specify a value for ``ResponsePagePath``. |
| `response_page_path` | String | No | The path to the custom error page that you want CloudFront to return to a viewer when your origin returns the HTTP status code specified by ``ErrorCode``, for example, ``/4xx-errors/403-forbidden.html``. If you want to store your objects and your custom error pages in different locations, your distribution must include a cache behavior for which the following is true: + The value of ``PathPattern`` matches the path to your custom error messages. For example, suppose you saved custom error pages for 4xx errors in an Amazon S3 bucket in a directory named ``/4xx-errors``. Your distribution must include a cache behavior for which the path pattern routes requests for your custom error pages to that location, for example, ``/4xx-errors/*``. + The value of ``TargetOriginId`` specifies the value of the ``ID`` element for the origin that contains your custom error pages. If you specify a value for ``ResponsePagePath``, you must also specify a value for ``ResponseCode``. We recommend that you store custom error pages in an Amazon S3 bucket. If you store custom error pages on an HTTP server and the server starts to return 5xx errors, CloudFront can't get the files that you want to return to viewers because the origin server is unavailable. |

### CustomOriginConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `http_port` | Int | No | The HTTP port that CloudFront uses to connect to the origin. Specify the HTTP port that the origin listens on. |
| `https_port` | Int | No | The HTTPS port that CloudFront uses to connect to the origin. Specify the HTTPS port that the origin listens on. |
| `ip_address_type` | [Enum (IpAddressType)](#ip_address_type-ipaddresstype) | No | Specifies which IP protocol CloudFront uses when connecting to your origin. If your origin uses both IPv4 and IPv6 protocols, you can choose ``dualstack`` to help optimize reliability. |
| `origin_keepalive_timeout` | Int | No | Specifies how long, in seconds, CloudFront persists its connection to the origin. The minimum timeout is 1 second, the maximum is 120 seconds, and the default (if you don't specify otherwise) is 5 seconds. For more information, see [Keep-alive timeout (custom origins only)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#DownloadDistValuesOriginKeepaliveTimeout) in the *Amazon CloudFront Developer Guide*. |
| `origin_mtls_config` | [Struct(OriginMtlsConfig)](#originmtlsconfig) | No | Configures mutual TLS authentication between CloudFront and your origin server. |
| `origin_protocol_policy` | [Enum (OriginProtocolPolicy)](#origin_protocol_policy-originprotocolpolicy) | Yes | Specifies the protocol (HTTP or HTTPS) that CloudFront uses to connect to the origin. Valid values are: + ``http-only`` ? CloudFront always uses HTTP to connect to the origin. + ``match-viewer`` ? CloudFront connects to the origin using the same protocol that the viewer used to connect to CloudFront. + ``https-only`` ? CloudFront always uses HTTPS to connect to the origin. |
| `origin_read_timeout` | Int | No | Specifies how long, in seconds, CloudFront waits for a response from the origin. This is also known as the *origin response timeout*. The minimum timeout is 1 second, the maximum is 120 seconds, and the default (if you don't specify otherwise) is 30 seconds. For more information, see [Response timeout](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#DownloadDistValuesOriginResponseTimeout) in the *Amazon CloudFront Developer Guide*. |
| `origin_ssl_protocols` | `List<String>` | No | Specifies the minimum SSL/TLS protocol that CloudFront uses when connecting to your origin over HTTPS. Valid values include ``SSLv3``, ``TLSv1``, ``TLSv1.1``, and ``TLSv1.2``. For more information, see [Minimum Origin SSL Protocol](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#DownloadDistValuesOriginSSLProtocols) in the *Amazon CloudFront Developer Guide*. |

### DefaultCacheBehavior

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `allowed_methods` | `List<String>` | No | A complex type that controls which HTTP methods CloudFront processes and forwards to your Amazon S3 bucket or your custom origin. There are three choices: + CloudFront forwards only ``GET`` and ``HEAD`` requests. + CloudFront forwards only ``GET``, ``HEAD``, and ``OPTIONS`` requests. + CloudFront forwards ``GET, HEAD, OPTIONS, PUT, PATCH, POST``, and ``DELETE`` requests. If you pick the third choice, you may need to restrict access to your Amazon S3 bucket or to your custom origin so users can't perform operations that you don't want them to. For example, you might not want users to have permissions to delete objects from your origin. |
| `cache_policy_id` | String | No | The unique identifier of the cache policy that is attached to the default cache behavior. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. A ``DefaultCacheBehavior`` must include either a ``CachePolicyId`` or ``ForwardedValues``. We recommend that you use a ``CachePolicyId``. |
| `cached_methods` | `List<String>` | No | A complex type that controls whether CloudFront caches the response to requests using the specified HTTP methods. There are two choices: + CloudFront caches responses to ``GET`` and ``HEAD`` requests. + CloudFront caches responses to ``GET``, ``HEAD``, and ``OPTIONS`` requests. If you pick the second choice for your Amazon S3 Origin, you may need to forward Access-Control-Request-Method, Access-Control-Request-Headers, and Origin headers for the responses to be cached correctly. |
| `compress` | Bool | No | Whether you want CloudFront to automatically compress certain files for this cache behavior. If so, specify ``true``; if not, specify ``false``. For more information, see [Serving Compressed Files](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/ServingCompressedFiles.html) in the *Amazon CloudFront Developer Guide*. |
| `default_ttl` | Float | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. This field is deprecated. We recommend that you use the ``DefaultTTL`` field in a cache policy instead of this field. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. The default amount of time that you want objects to stay in CloudFront caches before CloudFront forwards another request to your origin to determine whether the object has been updated. The value that you specify applies only when your origin does not add HTTP headers such as ``Cache-Control max-age``, ``Cache-Control s-maxage``, and ``Expires`` to objects. For more information, see [Managing How Long Content Stays in an Edge Cache (Expiration)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. |
| `field_level_encryption_id` | String | No | The value of ``ID`` for the field-level encryption configuration that you want CloudFront to use for encrypting specific fields of data for the default cache behavior. |
| `forwarded_values` | [Struct(ForwardedValues)](#forwardedvalues) | No | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. For more information, see [Working with policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/working-with-policies.html) in the *Amazon CloudFront Developer Guide*. If you want to include values in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. If you want to send values to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) or [Using the managed origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-origin-request-policies.html) in the *Amazon CloudFront Developer Guide*. A ``DefaultCacheBehavior`` must include either a ``CachePolicyId`` or ``ForwardedValues``. We recommend that you use a ``CachePolicyId``. A complex type that specifies how CloudFront handles query strings, cookies, and HTTP headers. |
| `function_associations` | [List\<FunctionAssociation\>](#functionassociation) | No | A list of CloudFront functions that are associated with this cache behavior. Your functions must be published to the ``LIVE`` stage to associate them with a cache behavior. |
| `grpc_config` | [Struct(GrpcConfig)](#grpcconfig) | No | The gRPC configuration for your cache behavior. |
| `lambda_function_associations` | [List\<LambdaFunctionAssociation\>](#lambdafunctionassociation) | No | A complex type that contains zero or more Lambda@Edge function associations for a cache behavior. |
| `max_ttl` | Float | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. This field is deprecated. We recommend that you use the ``MaxTTL`` field in a cache policy instead of this field. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. The maximum amount of time that you want objects to stay in CloudFront caches before CloudFront forwards another request to your origin to determine whether the object has been updated. The value that you specify applies only when your origin adds HTTP headers such as ``Cache-Control max-age``, ``Cache-Control s-maxage``, and ``Expires`` to objects. For more information, see [Managing How Long Content Stays in an Edge Cache (Expiration)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. |
| `min_ttl` | Float | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. This field is deprecated. We recommend that you use the ``MinTTL`` field in a cache policy instead of this field. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) or [Using the managed cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-cache-policies.html) in the *Amazon CloudFront Developer Guide*. The minimum amount of time that you want objects to stay in CloudFront caches before CloudFront forwards another request to your origin to determine whether the object has been updated. For more information, see [Managing How Long Content Stays in an Edge Cache (Expiration)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. You must specify ``0`` for ``MinTTL`` if you configure CloudFront to forward all headers to your origin (under ``Headers``, if you specify ``1`` for ``Quantity`` and ``*`` for ``Name``). |
| `origin_request_policy_id` | String | No | The unique identifier of the origin request policy that is attached to the default cache behavior. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) or [Using the managed origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-managed-origin-request-policies.html) in the *Amazon CloudFront Developer Guide*. |
| `realtime_log_config_arn` | Arn | No | The Amazon Resource Name (ARN) of the real-time log configuration that is attached to this cache behavior. For more information, see [Real-time logs](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/real-time-logs.html) in the *Amazon CloudFront Developer Guide*. |
| `response_headers_policy_id` | String | No | The identifier for a response headers policy. |
| `smooth_streaming` | Bool | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. Indicates whether you want to distribute media files in the Microsoft Smooth Streaming format using the origin that is associated with this cache behavior. If so, specify ``true``; if not, specify ``false``. If you specify ``true`` for ``SmoothStreaming``, you can still distribute other content using this cache behavior if the content matches the value of ``PathPattern``. |
| `target_origin_id` | String | Yes | The value of ``ID`` for the origin that you want CloudFront to route requests to when they use the default cache behavior. |
| `trusted_key_groups` | `List<String>` | No | A list of key groups that CloudFront can use to validate signed URLs or signed cookies. When a cache behavior contains trusted key groups, CloudFront requires signed URLs or signed cookies for all requests that match the cache behavior. The URLs or cookies must be signed with a private key whose corresponding public key is in the key group. The signed URL or cookie contains information about which public key CloudFront should use to verify the signature. For more information, see [Serving private content](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/PrivateContent.html) in the *Amazon CloudFront Developer Guide*. |
| `trusted_signers` | `List<String>` | No | We recommend using ``TrustedKeyGroups`` instead of ``TrustedSigners``. This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. A list of AWS-account IDs whose public keys CloudFront can use to validate signed URLs or signed cookies. When a cache behavior contains trusted signers, CloudFront requires signed URLs or signed cookies for all requests that match the cache behavior. The URLs or cookies must be signed with the private key of a CloudFront key pair in a trusted signer's AWS-account. The signed URL or cookie contains information about which public key CloudFront should use to verify the signature. For more information, see [Serving private content](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/PrivateContent.html) in the *Amazon CloudFront Developer Guide*. |
| `viewer_protocol_policy` | [Enum (ViewerProtocolPolicy)](#viewer_protocol_policy-viewerprotocolpolicy) | Yes | The protocol that viewers can use to access the files in the origin specified by ``TargetOriginId`` when a request matches the path pattern in ``PathPattern``. You can specify the following options: + ``allow-all``: Viewers can use HTTP or HTTPS. + ``redirect-to-https``: If a viewer submits an HTTP request, CloudFront returns an HTTP status code of 301 (Moved Permanently) to the viewer along with the HTTPS URL. The viewer then resubmits the request using the new URL. + ``https-only``: If a viewer sends an HTTP request, CloudFront returns an HTTP status code of 403 (Forbidden). For more information about requiring the HTTPS protocol, see [Requiring HTTPS Between Viewers and CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/using-https-viewers-to-cloudfront.html) in the *Amazon CloudFront Developer Guide*. The only way to guarantee that viewers retrieve an object that was fetched from the origin using HTTPS is never to use any other protocol to fetch the object. If you have recently changed from HTTP to HTTPS, we recommend that you clear your objects' cache because cached objects are protocol agnostic. That means that an edge location will return an object from the cache regardless of whether the current request protocol matches the protocol used previously. For more information, see [Managing Cache Expiration](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Expiration.html) in the *Amazon CloudFront Developer Guide*. |

### Definition

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `string_schema` | [Struct(StringSchema)](#stringschema) | No |  |

### DistributionConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `aliases` | `List<String>` | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. A complex type that contains information about CNAMEs (alternate domain names), if any, for this distribution. |
| `anycast_ip_list_id` | String | No | To use this field for a multi-tenant distribution, use a connection group instead. For more information, see [ConnectionGroup](https://docs.aws.amazon.com/cloudfront/latest/APIReference/API_ConnectionGroup.html). ID of the Anycast static IP list that is associated with the distribution. |
| `cnam_es` | `List<String>` | No | An alias for the CF distribution's domain name. This property is legacy. We recommend that you use [Aliases](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-cloudfront-distribution-distributionconfig.html#cfn-cloudfront-distribution-distributionconfig-aliases) instead. |
| `cache_behaviors` | [List\<CacheBehavior\>](#cachebehavior) | No | A complex type that contains zero or more ``CacheBehavior`` elements. |
| `cache_tag_config` | [Struct(CacheTagConfig)](#cachetagconfig) | No | Configuration for cache tag extraction from origin responses. When specified, CloudFront reads the header named in ``HeaderName`` from origin responses and stores the comma-separated values as cache tags on the object. Distributions without ``CacheTagConfig`` do not extract tags. When ``CacheTagConfig`` is removed from a distribution via ``UpdateDistribution``, CloudFront stops extracting tags from origin responses. Changing the ``HeaderName`` on an existing distribution does not retroactively affect previously cached objects. Tag-based invalidations will not apply to objects already cached using a previous header. To ensure tag invalidations function after updating the header name, use path-based invalidations to recache all objects that use cache tags. |
| `comment` | String | No | A comment to describe the distribution. The comment cannot be longer than 128 characters. |
| `connection_function_association` | [Struct(ConnectionFunctionAssociation)](#connectionfunctionassociation) | No | The distribution's connection function association. |
| `connection_mode` | [Enum (ConnectionMode)](#connection_mode-connectionmode) | No | This field specifies whether the connection mode is through a standard distribution (direct) or a multi-tenant distribution with distribution tenants (tenant-only). |
| `continuous_deployment_policy_id` | String | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. The identifier of a continuous deployment policy. For more information, see ``CreateContinuousDeploymentPolicy``. |
| `custom_error_responses` | [List\<CustomErrorResponse\>](#customerrorresponse) | No | A complex type that controls the following: + Whether CloudFront replaces HTTP status codes in the 4xx and 5xx range with custom error messages before returning the response to the viewer. + How long CloudFront caches HTTP status codes in the 4xx and 5xx range. For more information about custom error pages, see [Customizing Error Responses](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/custom-error-pages.html) in the *Amazon CloudFront Developer Guide*. |
| `custom_origin` | [Struct(LegacyCustomOrigin)](#legacycustomorigin) | No | The user-defined HTTP server that serves as the origin for content that CF distributes. This property is legacy. We recommend that you use [Origin](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-cloudfront-distribution-origin.html) instead. |
| `default_cache_behavior` | [Struct(DefaultCacheBehavior)](#defaultcachebehavior) | Yes | A complex type that describes the default cache behavior if you don't specify a ``CacheBehavior`` element or if files don't match any of the values of ``PathPattern`` in ``CacheBehavior`` elements. You must create exactly one default cache behavior. |
| `default_root_object` | String | No | When a viewer requests the root URL for your distribution, the default root object is the object that you want CloudFront to request from your origin. For example, if your root URL is ``https://www.example.com``, you can specify CloudFront to return the ``index.html`` file as the default root object. You can specify a default root object so that viewers see a specific file or object, instead of another object in your distribution (for example, ``https://www.example.com/product-description.html``). A default root object avoids exposing the contents of your distribution. You can specify the object name or a path to the object name (for example, ``index.html`` or ``exampleFolderName/index.html``). Your string can't begin with a forward slash (``/``). Only specify the object name or the path to the object. If you don't want to specify a default root object when you create a distribution, include an empty ``DefaultRootObject`` element. To delete the default root object from an existing distribution, update the distribution configuration and include an empty ``DefaultRootObject`` element. To replace the default root object, update the distribution configuration and specify the new object. For more information about the default root object, see [Specify a default root object](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DefaultRootObject.html) in the *Amazon CloudFront Developer Guide*. |
| `enabled` | Bool | Yes | From this field, you can enable or disable the selected distribution. |
| `http_version` | String | No | (Optional) Specify the HTTP version(s) that you want viewers to use to communicate with CF. The default value for new distributions is ``http1.1``. For viewers and CF to use HTTP/2, viewers must support TLSv1.2 or later, and must support Server Name Indication (SNI). For viewers and CF to use HTTP/3, viewers must support TLSv1.3 and Server Name Indication (SNI). CF supports HTTP/3 connection migration to allow the viewer to switch networks without losing connection. For more information about connection migration, see [Connection Migration](https://docs.aws.amazon.com/https://www.rfc-editor.org/rfc/rfc9000.html#name-connection-migration) at RFC 9000. For more information about supported TLSv1.3 ciphers, see [Supported protocols and ciphers between viewers and CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/secure-connections-supported-viewer-protocols-ciphers.html). |
| `ipv6_enabled` | Bool | No | To use this field for a multi-tenant distribution, use a connection group instead. For more information, see [ConnectionGroup](https://docs.aws.amazon.com/cloudfront/latest/APIReference/API_ConnectionGroup.html). If you want CloudFront to respond to IPv6 DNS requests with an IPv6 address for your distribution, specify ``true``. If you specify ``false``, CloudFront responds to IPv6 DNS requests with the DNS response code ``NOERROR`` and with no IP addresses. This allows viewers to submit a second request, for an IPv4 address for your distribution. In general, you should enable IPv6 if you have users on IPv6 networks who want to access your content. However, if you're using signed URLs or signed cookies to restrict access to your content, and if you're using a custom policy that includes the ``IpAddress`` parameter to restrict the IP addresses that can access your content, don't enable IPv6. If you want to restrict access to some content by IP address and not restrict access to other content (or restrict access but not by IP address), you can create two distributions. For more information, see [Creating a Signed URL Using a Custom Policy](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-creating-signed-url-custom-policy.html) in the *Amazon CloudFront Developer Guide*. If you're using an R53AWSIntlong alias resource record set to route traffic to your CloudFront distribution, you need to create a second alias resource record set when both of the following are true: + You enable IPv6 for the distribution + You're using alternate domain names in the URLs for your objects For more information, see [Routing Traffic to an Amazon CloudFront Web Distribution by Using Your Domain Name](https://docs.aws.amazon.com/Route53/latest/DeveloperGuide/routing-to-cloudfront-distribution.html) in the *Developer Guide*. If you created a CNAME resource record set, either with R53AWSIntlong or with another DNS service, you don't need to make any changes. A CNAME record will route traffic to your distribution regardless of the IP address format of the viewer request. |
| `logging` | [Struct(Logging)](#logging) | No | A complex type that controls whether access logs are written for the distribution. For more information about logging, see [Access Logs](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/AccessLogs.html) in the *Amazon CloudFront Developer Guide*. |
| `origin_groups` | [Struct(OriginGroups)](#origingroups) | No | A complex type that contains information about origin groups for this distribution. Specify a value for either the ``Origins`` or ``OriginGroups`` property. |
| `origins` | [List\<Origin\>](#origin) | No | A complex type that contains information about origins for this distribution. Specify a value for either the ``Origins`` or ``OriginGroups`` property. |
| `price_class` | String | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. The price class that corresponds with the maximum price that you want to pay for CloudFront service. If you specify ``PriceClass_All``, CloudFront responds to requests for your objects from all CloudFront edge locations. If you specify a price class other than ``PriceClass_All``, CloudFront serves your objects from the CloudFront edge location that has the lowest latency among the edge locations in your price class. Viewers who are in or near regions that are excluded from your specified price class may encounter slower performance. For more information about price classes, see [Choosing the Price Class for a CloudFront Distribution](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/PriceClass.html) in the *Amazon CloudFront Developer Guide*. For information about CloudFront pricing, including how price classes (such as Price Class 100) map to CloudFront regions, see [Amazon CloudFront Pricing](https://docs.aws.amazon.com/cloudfront/pricing/). |
| `restrictions` | [Struct(Restrictions)](#restrictions) | No | A complex type that identifies ways in which you want to restrict distribution of your content. |
| `s3_origin` | [Struct(LegacyS3Origin)](#legacys3origin) | No | The origin as an S3 bucket. This property is legacy. We recommend that you use [Origin](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-cloudfront-distribution-origin.html) instead. |
| `staging` | Bool | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. A Boolean that indicates whether this is a staging distribution. When this value is ``true``, this is a staging distribution. When this value is ``false``, this is not a staging distribution. |
| `tenant_config` | [Struct(TenantConfig)](#tenantconfig) | No | This field only supports multi-tenant distributions. You can't specify this field for standard distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. A distribution tenant configuration. |
| `viewer_certificate` | [Struct(ViewerCertificate)](#viewercertificate) | No | A complex type that determines the distribution's SSL/TLS configuration for communicating with viewers. |
| `viewer_mtls_config` | [Struct(ViewerMtlsConfig)](#viewermtlsconfig) | No | The distribution's viewer mTLS configuration. |
| `web_acl_id` | String | No | Multi-tenant distributions only support WAF V2 web ACLs. A unique identifier that specifies the WAF web ACL, if any, to associate with this distribution. To specify a web ACL created using the latest version of WAF, use the ACL ARN, for example ``arn:aws:wafv2:us-east-1:123456789012:global/webacl/ExampleWebACL/a1b2c3d4-5678-90ab-cdef-EXAMPLE11111``. To specify a web ACL created using WAF Classic, use the ACL ID, for example ``a1b2c3d4-5678-90ab-cdef-EXAMPLE11111``. WAF is a web application firewall that lets you monitor the HTTP and HTTPS requests that are forwarded to CloudFront, and lets you control access to your content. Based on conditions that you specify, such as the IP addresses that requests originate from or the values of query strings, CloudFront responds to requests either with the requested content or with an HTTP 403 status code (Forbidden). You can also configure CloudFront to return a custom error page when a request is blocked. For more information about WAF, see the [Developer Guide](https://docs.aws.amazon.com/waf/latest/developerguide/what-is-aws-waf.html). |

### ForwardedValues

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cookies` | [Struct(Cookies)](#cookies) | No | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. If you want to include cookies in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) in the *Amazon CloudFront Developer Guide*. If you want to send cookies to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) in the *Amazon CloudFront Developer Guide*. A complex type that specifies whether you want CloudFront to forward cookies to the origin and, if so, which ones. For more information about forwarding cookies to the origin, see [How CloudFront Forwards, Caches, and Logs Cookies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Cookies.html) in the *Amazon CloudFront Developer Guide*. |
| `headers` | `List<String>` | No | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. If you want to include headers in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) in the *Amazon CloudFront Developer Guide*. If you want to send headers to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) in the *Amazon CloudFront Developer Guide*. A complex type that specifies the ``Headers``, if any, that you want CloudFront to forward to the origin for this cache behavior (whitelisted headers). For the headers that you specify, CloudFront also caches separate versions of a specified object that is based on the header values in viewer requests. For more information, see [Caching Content Based on Request Headers](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/header-caching.html) in the *Amazon CloudFront Developer Guide*. |
| `query_string` | Bool | Yes | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. If you want to include query strings in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) in the *Amazon CloudFront Developer Guide*. If you want to send query strings to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) in the *Amazon CloudFront Developer Guide*. Indicates whether you want CloudFront to forward query strings to the origin that is associated with this cache behavior and cache based on the query string parameters. CloudFront behavior depends on the value of ``QueryString`` and on the values that you specify for ``QueryStringCacheKeys``, if any: If you specify true for ``QueryString`` and you don't specify any values for ``QueryStringCacheKeys``, CloudFront forwards all query string parameters to the origin and caches based on all query string parameters. Depending on how many query string parameters and values you have, this can adversely affect performance because CloudFront must forward more requests to the origin. If you specify true for ``QueryString`` and you specify one or more values for ``QueryStringCacheKeys``, CloudFront forwards all query string parameters to the origin, but it only caches based on the query string parameters that you specify. If you specify false for ``QueryString``, CloudFront doesn't forward any query string parameters to the origin, and doesn't cache based on query string parameters. For more information, see [Configuring CloudFront to Cache Based on Query String Parameters](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/QueryStringParameters.html) in the *Amazon CloudFront Developer Guide*. |
| `query_string_cache_keys` | `List<String>` | No | This field is deprecated. We recommend that you use a cache policy or an origin request policy instead of this field. If you want to include query strings in the cache key, use a cache policy. For more information, see [Creating cache policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-the-cache-key.html#cache-key-create-cache-policy) in the *Amazon CloudFront Developer Guide*. If you want to send query strings to the origin but not include them in the cache key, use an origin request policy. For more information, see [Creating origin request policies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/controlling-origin-requests.html#origin-request-create-origin-request-policy) in the *Amazon CloudFront Developer Guide*. A complex type that contains information about the query string parameters that you want CloudFront to use for caching for this cache behavior. |

### FunctionAssociation

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_type` | [Enum (FunctionAssociationEventType)](#event_type-functionassociationeventtype) | No | The event type of the function, either ``viewer-request`` or ``viewer-response``. You cannot use origin-facing event types (``origin-request`` and ``origin-response``) with a CloudFront function. |
| `function_arn` | Arn | No | The Amazon Resource Name (ARN) of the function. |

### GeoRestriction

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `locations` | `List<String>` | No | A complex type that contains a ``Location`` element for each country in which you want CloudFront either to distribute your content (``whitelist``) or not distribute your content (``blacklist``). The ``Location`` element is a two-letter, uppercase country code for a country that you want to include in your ``blacklist`` or ``whitelist``. Include one ``Location`` element for each country. CloudFront and ``MaxMind`` both use ``ISO 3166`` country codes. For the current list of countries and the corresponding codes, see ``ISO 3166-1-alpha-2`` code on the *International Organization for Standardization* website. You can also refer to the country list on the CloudFront console, which includes both country names and codes. |
| `restriction_type` | [Enum (RestrictionType)](#restriction_type-restrictiontype) | Yes | The method that you want to use to restrict distribution of your content by country: + ``none``: No geo restriction is enabled, meaning access to content is not restricted by client geo location. + ``blacklist``: The ``Location`` elements specify the countries in which you don't want CloudFront to distribute your content. + ``whitelist``: The ``Location`` elements specify the countries in which you want CloudFront to distribute your content. |

### GrpcConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `enabled` | Bool | Yes | Enables your CloudFront distribution to receive gRPC requests and to proxy them directly to your origins. |

### LambdaFunctionAssociation

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_type` | [Enum (LambdaFunctionAssociationEventType)](#event_type-lambdafunctionassociationeventtype) | No | Specifies the event type that triggers a Lambda@Edge function invocation. You can specify the following values: + ``viewer-request``: The function executes when CloudFront receives a request from a viewer and before it checks to see whether the requested object is in the edge cache. + ``origin-request``: The function executes only when CloudFront sends a request to your origin. When the requested object is in the edge cache, the function doesn't execute. + ``origin-response``: The function executes after CloudFront receives a response from the origin and before it caches the object in the response. When the requested object is in the edge cache, the function doesn't execute. + ``viewer-response``: The function executes before CloudFront returns the requested object to the viewer. The function executes regardless of whether the object was already in the edge cache. If the origin returns an HTTP status code other than HTTP 200 (OK), the function doesn't execute. |
| `include_body` | Bool | No | A flag that allows a Lambda@Edge function to have read access to the body content. For more information, see [Accessing the Request Body by Choosing the Include Body Option](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/lambda-include-body-access.html) in the Amazon CloudFront Developer Guide. |
| `lambda_function_arn` | Arn | No | The ARN of the Lambda@Edge function. You must specify the ARN of a function version; you can't specify an alias or $LATEST. |

### LegacyCustomOrigin

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `dns_name` | String | Yes | The domain name assigned to your CF distribution. |
| `http_port` | Int | No | The HTTP port that CF uses to connect to the origin. Specify the HTTP port that the origin listens on. |
| `https_port` | Int | No | The HTTPS port that CF uses to connect to the origin. Specify the HTTPS port that the origin listens on. |
| `origin_protocol_policy` | String | Yes | Specifies the protocol (HTTP or HTTPS) that CF uses to connect to the origin. |
| `origin_ssl_protocols` | `List<String>` | Yes | The minimum SSL/TLS protocol version that CF uses when communicating with your origin server over HTTPs. For more information, see [Minimum Origin SSL Protocol](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#DownloadDistValuesOriginSSLProtocols) in the *Developer Guide*. |

### LegacyS3Origin

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `dns_name` | String | Yes | The domain name assigned to your CF distribution. |
| `origin_access_identity` | String | No | The CF origin access identity to associate with the distribution. Use an origin access identity to configure the distribution so that end users can only access objects in an S3 through CF. This property is legacy. We recommend that you use [OriginAccessControl](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-cloudfront-originaccesscontrol.html) instead. |

### Logging

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `bucket` | String | No | The Amazon S3 bucket to store the access logs in, for example, ``amzn-s3-demo-bucket.s3.amazonaws.com``. |
| `include_cookies` | Bool | No | Specifies whether you want CloudFront to include cookies in access logs, specify ``true`` for ``IncludeCookies``. If you choose to include cookies in logs, CloudFront logs all cookies regardless of how you configure the cache behaviors for this distribution. If you don't want to include cookies when you create a distribution or if you want to disable include cookies for an existing distribution, specify ``false`` for ``IncludeCookies``. |
| `prefix` | String | No | An optional string that you want CloudFront to prefix to the access log ``filenames`` for this distribution, for example, ``myprefix/``. If you want to enable logging, but you don't want to specify a prefix, you still must include an empty ``Prefix`` element in the ``Logging`` element. |

### Origin

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `connection_attempts` | Int | No | The number of times that CloudFront attempts to connect to the origin. The minimum number is 1, the maximum is 3, and the default (if you don't specify otherwise) is 3. For a custom origin (including an Amazon S3 bucket that's configured with static website hosting), this value also specifies the number of times that CloudFront attempts to get a response from the origin, in the case of an [Origin Response Timeout](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#DownloadDistValuesOriginResponseTimeout). For more information, see [Origin Connection Attempts](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#origin-connection-attempts) in the *Amazon CloudFront Developer Guide*. |
| `connection_timeout` | Int | No | The number of seconds that CloudFront waits when trying to establish a connection to the origin. The minimum timeout is 1 second, the maximum is 10 seconds, and the default (if you don't specify otherwise) is 10 seconds. For more information, see [Origin Connection Timeout](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#origin-connection-timeout) in the *Amazon CloudFront Developer Guide*. |
| `custom_origin_config` | [Struct(CustomOriginConfig)](#customoriginconfig) | No | Use this type to specify an origin that is not an Amazon S3 bucket, with one exception. If the Amazon S3 bucket is configured with static website hosting, use this type. If the Amazon S3 bucket is not configured with static website hosting, use the ``S3OriginConfig`` type instead. |
| `domain_name` | String | Yes | The domain name for the origin. For more information, see [Origin Domain Name](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#DownloadDistValuesDomainName) in the *Amazon CloudFront Developer Guide*. |
| `id` | String | Yes | A unique identifier for the origin. This value must be unique within the distribution. Use this value to specify the ``TargetOriginId`` in a ``CacheBehavior`` or ``DefaultCacheBehavior``. |
| `origin_access_control_id` | String | No | The unique identifier of an origin access control for this origin. For more information, see [Restricting access to an Amazon S3 origin](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-s3.html) in the *Amazon CloudFront Developer Guide*. |
| `origin_custom_headers` | [List\<OriginCustomHeader\>](#origincustomheader) | No | A list of HTTP header names and values that CloudFront adds to the requests that it sends to the origin. For more information, see [Adding Custom Headers to Origin Requests](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/add-origin-custom-headers.html) in the *Amazon CloudFront Developer Guide*. |
| `origin_path` | String | No | An optional path that CloudFront appends to the origin domain name when CloudFront requests content from the origin. For more information, see [Origin Path](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#DownloadDistValuesOriginPath) in the *Amazon CloudFront Developer Guide*. |
| `origin_shield` | [Struct(OriginShield)](#originshield) | No | CloudFront Origin Shield. Using Origin Shield can help reduce the load on your origin. For more information, see [Using Origin Shield](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/origin-shield.html) in the *Amazon CloudFront Developer Guide*. |
| `response_completion_timeout` | Int | No | The time (in seconds) that a request from CloudFront to the origin can stay open and wait for a response. If the complete response isn't received from the origin by this time, CloudFront ends the connection. The value for ``ResponseCompletionTimeout`` must be equal to or greater than the value for ``OriginReadTimeout``. If you don't set a value for ``ResponseCompletionTimeout``, CloudFront doesn't enforce a maximum value. For more information, see [Response completion timeout](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#response-completion-timeout) in the *Amazon CloudFront Developer Guide*. |
| `s3_origin_config` | [Struct(S3OriginConfig)](#s3originconfig) | No | Use this type to specify an origin that is an Amazon S3 bucket that is not configured with static website hosting. To specify any other type of origin, including an Amazon S3 bucket that is configured with static website hosting, use the ``CustomOriginConfig`` type instead. |
| `vpc_origin_config` | [Struct(VpcOriginConfig)](#vpcoriginconfig) | No | The VPC origin configuration. |

### OriginCustomHeader

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `header_name` | String | Yes | The name of a header that you want CloudFront to send to your origin. For more information, see [Adding Custom Headers to Origin Requests](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/forward-custom-headers.html) in the *Amazon CloudFront Developer Guide*. |
| `header_value` | String | Yes | The value for the header that you specified in the ``HeaderName`` field. |

### OriginGroup

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `failover_criteria` | [Struct(OriginGroupFailoverCriteria)](#origingroupfailovercriteria) | Yes | A complex type that contains information about the failover criteria for an origin group. |
| `id` | String | Yes | The origin group's ID. |
| `members` | [Struct(OriginGroupMembers)](#origingroupmembers) | Yes | A complex type that contains information about the origins in an origin group. |
| `selection_criteria` | [Enum (OriginGroupSelectionCriteria)](#selection_criteria-origingroupselectioncriteria) | No | The selection criteria for the origin group. For more information, see [Create an origin group](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/high_availability_origin_failover.html#concept_origin_groups.creating) in the *Amazon CloudFront Developer Guide*. |

### OriginGroupFailoverCriteria

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status_codes` | [Struct(StatusCodes)](#statuscodes) | Yes | The status codes that, when returned from the primary origin, will trigger CloudFront to failover to the second origin. |

### OriginGroupMember

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `origin_id` | String | Yes | The ID for an origin in an origin group. |

### OriginGroupMembers

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `items` | [List\<OriginGroupMember\>](#origingroupmember) | Yes | Items (origins) in an origin group. |
| `quantity` | Int | Yes | The number of origins in an origin group. |

### OriginGroups

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `items` | [List\<OriginGroup\>](#origingroup) | No | The items (origin groups) in a distribution. |
| `quantity` | Int | Yes | The number of origin groups. |

### OriginMtlsConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `client_certificate_arn` | Arn | Yes | The Amazon Resource Name (ARN) of the client certificate stored in AWS Certificate Manager (ACM) that CloudFront uses to authenticate with your origin using Mutual TLS. |

### OriginShield

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `enabled` | Bool | No | A flag that specifies whether Origin Shield is enabled. When it's enabled, CloudFront routes all requests through Origin Shield, which can help protect your origin. When it's disabled, CloudFront might send requests directly to your origin from multiple edge locations or regional edge caches. |
| `origin_shield_region` | Region | No | The AWS-Region for Origin Shield. Specify the AWS-Region that has the lowest latency to your origin. To specify a region, use the region code, not the region name. For example, specify the US East (Ohio) region as ``us-east-2``. When you enable CloudFront Origin Shield, you must specify the AWS-Region for Origin Shield. For the list of AWS-Regions that you can specify, and for help choosing the best Region for your origin, see [Choosing the for Origin Shield](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/origin-shield.html#choose-origin-shield-region) in the *Amazon CloudFront Developer Guide*. |

### ParameterDefinition

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `definition` | [Struct(Definition)](#definition) | Yes | The value that you assigned to the parameter. |
| `name` | String | Yes | The name of the parameter. |

### Restrictions

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `geo_restriction` | [Struct(GeoRestriction)](#georestriction) | Yes | A complex type that controls the countries in which your content is distributed. CF determines the location of your users using ``MaxMind`` GeoIP databases. To disable geo restriction, remove the [Restrictions](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-properties-cloudfront-distribution-distributionconfig.html#cfn-cloudfront-distribution-distributionconfig-restrictions) property from your stack template. |

### S3OriginConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `origin_access_identity` | String | No | If you're using origin access control (OAC) instead of origin access identity, specify an empty ``OriginAccessIdentity`` element. For more information, see [Restricting access to an](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-origin.html) in the *Amazon CloudFront Developer Guide*. The CloudFront origin access identity to associate with the origin. Use an origin access identity to configure the origin so that viewers can *only* access objects in an Amazon S3 bucket through CloudFront. The format of the value is: ``origin-access-identity/cloudfront/ID-of-origin-access-identity`` The ``ID-of-origin-access-identity`` is the value that CloudFront returned in the ``ID`` element when you created the origin access identity. If you want viewers to be able to access objects using either the CloudFront URL or the Amazon S3 URL, specify an empty ``OriginAccessIdentity`` element. To delete the origin access identity from an existing distribution, update the distribution configuration and include an empty ``OriginAccessIdentity`` element. To replace the origin access identity, update the distribution configuration and specify the new origin access identity. For more information about the origin access identity, see [Serving Private Content through CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/PrivateContent.html) in the *Amazon CloudFront Developer Guide*. |
| `origin_read_timeout` | Int | No | Specifies how long, in seconds, CloudFront waits for a response from the origin. This is also known as the *origin response timeout*. The minimum timeout is 1 second, the maximum is 120 seconds, and the default (if you don't specify otherwise) is 30 seconds. For more information, see [Response timeout](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#DownloadDistValuesOriginResponseTimeout) in the *Amazon CloudFront Developer Guide*. |

### StatusCodes

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `items` | `List<Int>` | Yes | The items (status codes) for an origin group. |
| `quantity` | Int | Yes | The number of status codes. |

### StringSchema

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `comment` | String | No |  |
| `default_value` | String | No |  |
| `required` | Bool | Yes |  |

### TenantConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `parameter_definitions` | [List\<ParameterDefinition\>](#parameterdefinition) | No |  |

### TrustStoreConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `advertise_trust_store_ca_names` | Bool | No | The configuration to use to advertise trust store CA names. |
| `ignore_certificate_expiry` | Bool | No | The configuration to use to ignore certificate expiration. |
| `trust_store_id` | String | Yes | The trust store ID. |

### ViewerCertificate

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acm_certificate_arn` | Arn | No | In CloudFormation, this field name is ``AcmCertificateArn``. Note the different capitalization. If the distribution uses ``Aliases`` (alternate domain names or CNAMEs) and the SSL/TLS certificate is stored in [(ACM)](https://docs.aws.amazon.com/acm/latest/userguide/acm-overview.html), provide the Amazon Resource Name (ARN) of the ACM certificate. CloudFront only supports ACM certificates in the US East (N. Virginia) Region (``us-east-1``). If you specify an ACM certificate ARN, you must also specify values for ``MinimumProtocolVersion`` and ``SSLSupportMethod``. (In CloudFormation, the field name is ``SslSupportMethod``. Note the different capitalization.) |
| `cloud_front_default_certificate` | Bool | No | If the distribution uses the CloudFront domain name such as ``d111111abcdef8.cloudfront.net``, set this field to ``true``. If the distribution uses ``Aliases`` (alternate domain names or CNAMEs), omit this field and specify values for the following fields: + ``AcmCertificateArn`` or ``IamCertificateId`` (specify a value for one, not both) + ``MinimumProtocolVersion`` + ``SslSupportMethod`` |
| `iam_certificate_id` | String | No | This field only supports standard distributions. You can't specify this field for multi-tenant distributions. For more information, see [Unsupported features for SaaS Manager for Amazon CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-config-options.html#unsupported-saas) in the *Amazon CloudFront Developer Guide*. In CloudFormation, this field name is ``IamCertificateId``. Note the different capitalization. If the distribution uses ``Aliases`` (alternate domain names or CNAMEs) and the SSL/TLS certificate is stored in [(IAM)](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_server-certs.html), provide the ID of the IAM certificate. If you specify an IAM certificate ID, you must also specify values for ``MinimumProtocolVersion`` and ``SSLSupportMethod``. (In CloudFormation, the field name is ``SslSupportMethod``. Note the different capitalization.) |
| `minimum_protocol_version` | [Enum (MinimumProtocolVersion)](#minimum_protocol_version-minimumprotocolversion) | No | If the distribution uses ``Aliases`` (alternate domain names or CNAMEs), specify the security policy that you want CloudFront to use for HTTPS connections with viewers. The security policy determines two settings: + The minimum SSL/TLS protocol that CloudFront can use to communicate with viewers. + The ciphers that CloudFront can use to encrypt the content that it returns to viewers. For more information, see [Security Policy](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/distribution-web-values-specify.html#DownloadDistValues-security-policy) and [Supported Protocols and Ciphers Between Viewers and CloudFront](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/secure-connections-supported-viewer-protocols-ciphers.html#secure-connections-supported-ciphers) in the *Amazon CloudFront Developer Guide*. On the CloudFront console, this setting is called *Security Policy*. When you're using SNI only (you set ``SSLSupportMethod`` to ``sni-only``), you must specify ``TLSv1`` or higher. (In CloudFormation, the field name is ``SslSupportMethod``. Note the different capitalization.) If the distribution uses the CloudFront domain name such as ``d111111abcdef8.cloudfront.net`` (you set ``CloudFrontDefaultCertificate`` to ``true``), CloudFront automatically sets the security policy to ``TLSv1`` regardless of the value that you set here. |
| `ssl_support_method` | [Enum (SslSupportMethod)](#ssl_support_method-sslsupportmethod) | No | In CloudFormation, this field name is ``SslSupportMethod``. Note the different capitalization. If the distribution uses ``Aliases`` (alternate domain names or CNAMEs), specify which viewers the distribution accepts HTTPS connections from. + ``sni-only`` ? The distribution accepts HTTPS connections from only viewers that support [server name indication (SNI)](https://docs.aws.amazon.com/https://en.wikipedia.org/wiki/Server_Name_Indication). This is recommended. Most browsers and clients support SNI. + ``vip`` ? The distribution accepts HTTPS connections from all viewers including those that don't support SNI. This is not recommended, and results in additional monthly charges from CloudFront. + ``static-ip`` - Do not specify this value unless your distribution has been enabled for this feature by the CloudFront team. If you have a use case that requires static IP addresses for a distribution, contact CloudFront through the [Center](https://docs.aws.amazon.com/support/home). If the distribution uses the CloudFront domain name such as ``d111111abcdef8.cloudfront.net``, don't set a value for this field. |

### ViewerMtlsConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `mode` | [Enum (ViewerMtlsMode)](#mode-viewermtlsmode) | No | The viewer mTLS mode. |
| `trust_store_config` | [Struct(TrustStoreConfig)](#truststoreconfig) | No | The trust store configuration associated with the viewer mTLS configuration. |

### VpcOriginConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `origin_keepalive_timeout` | Int | No | Specifies how long, in seconds, CloudFront persists its connection to the origin. The minimum timeout is 1 second, the maximum is 120 seconds, and the default (if you don't specify otherwise) is 5 seconds. For more information, see [Keep-alive timeout (custom origins only)](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#DownloadDistValuesOriginKeepaliveTimeout) in the *Amazon CloudFront Developer Guide*. |
| `origin_read_timeout` | Int | No | Specifies how long, in seconds, CloudFront waits for a response from the origin. This is also known as the *origin response timeout*. The minimum timeout is 1 second, the maximum is 120 seconds, and the default (if you don't specify otherwise) is 30 seconds. For more information, see [Response timeout](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/DownloadDistValuesOrigin.html#DownloadDistValuesOriginResponseTimeout) in the *Amazon CloudFront Developer Guide*. |
| `owner_account_id` | AwsAccountId | No | The account ID of the AWS-account that owns the VPC origin. |
| `vpc_origin_id` | String | Yes | The VPC origin ID. |

## Attribute Reference

### `domain_name`

- **Type:** String



### `id`

- **Type:** String



