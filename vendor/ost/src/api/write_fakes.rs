//! §GRAPH2: fake-transport round trips for the five writes GRAPHSWEEP3
//! moved to Teams services (create team, edit channel, delete channel,
//! add app for yourself, hide chat). Each runs the real function against
//! a loopback fake ([`fake_transport`]) and asserts the method, path,
//! auth headers (right token to the right service, no token to the
//! wrong one) and JSON body. Nothing here touches a tenant.

use serde_json::json;

use super::apps::{INSTALL_CALLER, install_app_for_user};
use super::chat::{
    delete_message_with_client, reply_rich_message_with_client_id,
    mark_read_with_client, read_receipts_data, remove_reaction_with_client,
    send_reaction_with_client, send_typing_indicator_with_client,
    set_chat_hidden_with_client, set_chat_muted_with_client,
    thread_reply_rich_with_client_id,
};
use super::fake_transport::{AAD, Fake, GRAPH, Recorded, SKYPE};
use super::teams::{
    create_team_body, create_team_data, delete_channel_body, delete_channel_data,
    update_channel_data,
};

const TEAM_GUID: &str = "11111111-2222-3333-4444-555555555555";
const TEAM_THREAD: &str = "19:team@thread.tacv2";
const CHANNEL: &str = "19:chan@thread.tacv2";

/// The middle tier takes AAD Bearer + X-Skypetoken; neither the Graph
/// token nor a chat-service header goes with it.
fn assert_mt_auth(r: &Recorded) {
    assert_eq!(
        r.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {AAD}").as_str())
    );
    assert_eq!(
        r.headers.get("x-skypetoken").map(String::as_str),
        Some(SKYPE)
    );
    assert!(
        r.headers
            .get("content-type")
            .is_some_and(|v| v.contains("application/json"))
    );
    assert!(
        !r.haystack().contains(GRAPH),
        "Graph token must not reach the middle tier: {}",
        r.method
    );
    assert!(!r.headers.contains_key("authentication"));
}

/// Graph reads carry only the Graph token.
fn assert_graph_auth(r: &Recorded) {
    assert_eq!(
        r.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {GRAPH}").as_str())
    );
    assert!(!r.haystack().contains(AAD) && !r.haystack().contains(SKYPE));
}

fn team_lookup_route() -> (&'static str, &'static str, u16, String) {
    (
        "GET",
        "/graph/teams/11111111",
        200,
        json!({"internalId": TEAM_THREAD}).to_string(),
    )
}

#[tokio::test]
async fn create_team_posts_web_body_to_middle_tier() {
    let fake = Fake::start(vec![
        (
            "POST",
            "/mt/beta/teams/create",
            200,
            json!({"value": {"siteInfo": {"groupId": TEAM_GUID}, "skypeThreadId": TEAM_THREAD}})
                .to_string(),
        ),
        (
            "GET",
            "/graph/teams/11111111-2222-3333-4444-555555555555/channels",
            200,
            json!({"value": []}).to_string(),
        ),
        (
            "GET",
            "/graph/teams/11111111",
            200,
            json!({"id": TEAM_GUID, "displayName": "Squad"}).to_string(),
        ),
    ])
    .await;
    let out = create_team_data(&fake.client(), "  Squad ", Some("Ship it"))
        .await
        .expect("create ok");
    assert_eq!(out.team.id, TEAM_GUID);
    let reqs = fake.requests();
    let post = &reqs[0];
    assert_eq!(
        (post.method.as_str(), post.path.as_str()),
        ("POST", "/mt/beta/teams/create")
    );
    assert_mt_auth(post);
    assert_eq!(post.json(), create_team_body("Squad", Some("Ship it")));
    assert_eq!(post.json()["accessType"], 1);
    // The read-back is Graph (Team.ReadBasic.All), Graph token only.
    assert!(reqs.len() >= 2);
    for g in &reqs[1..] {
        assert_eq!(g.method, "GET");
        assert!(g.path.starts_with("/graph/teams/"));
        assert_graph_auth(g);
    }
}

#[tokio::test]
async fn create_team_refusal_is_an_error_and_never_falls_back_to_graph() {
    let fake = Fake::start(vec![(
        "POST",
        "/mt/beta/teams/create",
        403,
        "{\"error\":\"denied\"}".into(),
    )])
    .await;
    let err = create_team_data(&fake.client(), "Squad", None)
        .await
        .err()
        .expect("403 must surface");
    assert!(format!("{err:#}").contains("403"), "{err:#}");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1, "no Graph create fallback");
}

#[tokio::test]
async fn add_reaction_posts_native_type_with_chat_service_auth() {
    let fake = Fake::start(vec![(
        "POST",
        "/chat/v1/users/ME/conversations/chat:ana/messages/msg-1/reactions",
        200,
        "{}".into(),
    )])
    .await;
    send_reaction_with_client(&fake.client(), "chat:ana", "msg-1", "👍")
        .await
        .expect("reaction accepted by fixture");
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        "/chat/v1/users/ME/conversations/chat:ana/messages/msg-1/reactions"
    );
    assert_eq!(request.json(), json!({"reactionType": "like"}));
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!request.haystack().contains(GRAPH));
    assert!(!request.headers.contains_key("authorization"));
}

#[tokio::test]
async fn remove_reaction_deletes_native_type_and_surfaces_server_failure() {
    let route = "/chat/v1/users/ME/conversations/chat-1/messages/msg-2/reactions/heart";
    let fake = Fake::start(vec![("DELETE", route, 403, "{\"error\":\"denied\"}".into())]).await;
    let error = remove_reaction_with_client(&fake.client(), "chat-1", "msg-2", "❤️")
        .await
        .expect_err("server refusal must be visible");
    assert!(format!("{error:#}").contains("403"));
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "DELETE");
    assert_eq!(requests[0].path, route);
    assert_eq!(
        requests[0].headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(requests[0].body.is_empty());
}

#[tokio::test]
async fn unknown_reaction_is_rejected_before_network() {
    let fake = Fake::start(vec![]).await;
    assert!(send_reaction_with_client(&fake.client(), "chat-1", "msg-3", "🔥")
        .await
        .is_err());
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn mark_read_puts_the_selected_message_as_consumption_frontier() {
    let route = "/chat/v1/users/ME/conversations/chat-1/properties?name=consumptionhorizon";
    let fake = Fake::start(vec![("PUT", route, 200, "{}".into())]).await;
    mark_read_with_client(&fake.client(), "chat-1", "message-9")
        .await
        .expect("local read marker accepted");
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(request.path, route);
    let body = request.json();
    let horizon = body["consumptionhorizon"]
        .as_str()
        .expect("horizon body");
    let mut parts = horizon.split(';');
    assert_eq!(parts.next(), parts.next());
    assert_eq!(parts.next(), Some("message-9"));
    assert!(parts.next().is_none());
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!request.haystack().contains(GRAPH));
}

#[tokio::test]
async fn read_receipts_get_returns_only_server_reported_horizons() {
    let route = "/chat/v1/threads/chat-1/consumptionhorizons";
    let fake = Fake::start(vec![(
        "GET",
        route,
        200,
        json!({"consumptionhorizons": [
            {"mri":"8:orgid:peer", "consumptionhorizon":"10;10;message-7"}
        ]})
        .to_string(),
    )])
    .await;
    let receipts = read_receipts_data(&fake.client(), "chat-1")
        .await
        .expect("local read receipt response");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].user, "8:orgid:peer");
    assert_eq!(receipts[0].message_id, "message-7");
    let request = &fake.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, route);
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
}

#[tokio::test]
async fn edit_channel_patches_middle_tier_channel_url() {
    let fake = Fake::start(vec![
        team_lookup_route(),
        ("PATCH", "/mt/beta/teams/", 200, "{}".into()),
    ])
    .await;
    update_channel_data(
        &fake.client(),
        TEAM_GUID,
        CHANNEL,
        Some(" Launch "),
        Some("Plans"),
    )
    .await
    .expect("edit ok");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[0].path,
        format!("/graph/teams/{TEAM_GUID}?$select=internalId")
    );
    assert_graph_auth(&reqs[0]);
    let patch = &reqs[1];
    assert_eq!(patch.method, "PATCH");
    assert_eq!(
        patch.path,
        "/mt/beta/teams/19%3Ateam%40thread.tacv2/channels/19%3Achan%40thread.tacv2"
    );
    assert_mt_auth(patch);
    assert!(!patch.headers.contains_key("x-ms-client-caller"));
    assert_eq!(
        patch.json(),
        json!({"displayName": "Launch", "description": "Plans"})
    );
}

#[tokio::test]
async fn edit_channel_with_nothing_to_change_sends_nothing() {
    let fake = Fake::start(vec![]).await;
    assert!(
        update_channel_data(&fake.client(), TEAM_GUID, CHANNEL, None, None)
            .await
            .is_err()
    );
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn delete_channel_sends_descriptor_body_to_middle_tier() {
    let fake = Fake::start(vec![
        team_lookup_route(),
        ("DELETE", "/mt/beta/teams/", 200, "{}".into()),
    ])
    .await;
    delete_channel_data(&fake.client(), TEAM_GUID, CHANNEL)
        .await
        .expect("delete ok");
    let reqs = fake.requests();
    let del = &reqs[1];
    assert_eq!(del.method, "DELETE");
    assert_eq!(
        del.path,
        "/mt/beta/teams/19%3Ateam%40thread.tacv2/channels/19%3Achan%40thread.tacv2"
    );
    assert_mt_auth(del);
    // INFERRED: the web client sends the whole channel descriptor as the
    // DELETE body; this is only the subset the app knows (id, host team
    // thread + group id, isGeneral). Not yet proven against a live tenant.
    assert_eq!(
        del.json(),
        json!({"id": CHANNEL, "hostTeamId": TEAM_THREAD, "hostTeamGroupId": TEAM_GUID, "isGeneral": false})
    );
    assert_eq!(
        del.json(),
        delete_channel_body(CHANNEL, TEAM_THREAD, TEAM_GUID)
    );
}

#[tokio::test]
async fn delete_general_channel_is_refused_before_the_middle_tier() {
    let fake = Fake::start(vec![team_lookup_route()]).await;
    let err = delete_channel_data(&fake.client(), TEAM_GUID, TEAM_THREAD)
        .await
        .expect_err("General refused");
    assert!(format!("{err:#}").contains("General"), "{err:#}");
    assert!(fake.requests().iter().all(|r| !r.path.starts_with("/mt/")));
}

#[tokio::test]
async fn add_app_for_self_posts_definition_to_entitlements() {
    let def = json!({"id": "app-1", "name": {"short": "Sample"}, "version": "1.0.0"});
    let fake = Fake::start(vec![
        (
            "POST",
            "/mt/beta/users/apps/batchedDefinitions",
            200,
            json!({"definitions": [def]}).to_string(),
        ),
        ("POST", "/mt/beta/users/apps/entitlements", 200, "{}".into()),
    ])
    .await;
    install_app_for_user(&fake.client(), " app-1 ")
        .await
        .expect("install ok");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].json(), json!(["app-1"]));
    assert_mt_auth(&reqs[0]);
    let post = &reqs[1];
    assert_eq!(
        (post.method.as_str(), post.path.as_str()),
        ("POST", "/mt/beta/users/apps/entitlements")
    );
    assert_mt_auth(post);
    assert_eq!(
        post.headers.get("x-ms-client-caller").map(String::as_str),
        Some(INSTALL_CALLER)
    );
    assert_eq!(post.json(), def, "body is the catalog definition");
}

#[tokio::test]
async fn add_app_missing_from_catalog_never_posts_entitlement() {
    let fake = Fake::start(vec![(
        "POST",
        "/mt/beta/users/apps/batchedDefinitions",
        200,
        json!({"definitions": []}).to_string(),
    )])
    .await;
    assert!(install_app_for_user(&fake.client(), "app-1").await.is_err());
    assert!(
        fake.requests()
            .iter()
            .all(|r| !r.path.ends_with("/entitlements"))
    );
}

const CHAT: &str = "19:chat@thread.v2";

#[tokio::test]
async fn typing_indicator_uses_the_native_transient_control_message() {
    let fake = Fake::start(vec![(
        "POST",
        "/chat/v1/users/ME/conversations/",
        200,
        "{}".into(),
    )])
    .await;
    send_typing_indicator_with_client(&fake.client(), CHAT)
        .await
        .expect("typing indicator accepted");
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        format!("/chat/v1/users/ME/conversations/{CHAT}/messages")
    );
    assert_eq!(
        request.json(),
        json!({
            "messagetype": "Control/Typing",
            "contenttype": "Application/Message",
            "content": ""
        })
    );
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!request.headers.contains_key("authorization"));
}

#[tokio::test]
async fn mute_chat_updates_alerts_with_the_chat_service_token() {
    let fake = Fake::start(vec![(
        "PUT",
        "/chat/v1/users/ME/conversations/",
        200,
        "{}".into(),
    )])
    .await;
    set_chat_muted_with_client(&fake.client(), CHAT, true)
        .await
        .expect("mute ok");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    let request = &reqs[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(
        request.path,
        format!("/chat/v1/users/ME/conversations/{CHAT}/properties?name=alerts")
    );
    assert_eq!(request.json(), json!({"alerts": "false"}));
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!request.headers.contains_key("authorization"));
    assert!(!request.haystack().contains(AAD) && !request.haystack().contains(GRAPH));
}

#[tokio::test]
async fn hide_chat_puts_two_properties_on_chat_service() {
    let fake = Fake::start(vec![(
        "PUT",
        "/chat/v1/users/ME/conversations/",
        200,
        "{}".into(),
    )])
    .await;
    set_chat_hidden_with_client(&fake.client(), CHAT, true)
        .await
        .expect("hide ok");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert_eq!(r.method, "PUT");
        assert_eq!(
            r.headers.get("authentication").map(String::as_str),
            Some(format!("skypetoken={SKYPE}").as_str())
        );
        assert!(
            !r.headers.contains_key("authorization"),
            "chat service takes the skype token only"
        );
        assert!(!r.haystack().contains(AAD) && !r.haystack().contains(GRAPH));
    }
    assert_eq!(
        reqs[0].path,
        format!("/chat/v1/users/ME/conversations/{CHAT}/properties?name=unpinnedTime")
    );
    assert_eq!(
        reqs[1].path,
        format!("/chat/v1/users/ME/conversations/{CHAT}/properties?name=historyHiddenTime")
    );
    let ms = reqs[0].json()["unpinnedTime"]
        .as_u64()
        .expect("unpinnedTime is a number (web client)");
    assert!(ms > 1_600_000_000_000, "epoch millis");
    assert_eq!(
        reqs[1].json(),
        json!({"historyHiddenTime": ms.to_string()}),
        "same instant, as a string"
    );
}

#[tokio::test]
async fn unhide_chat_clears_unpinned_time_only() {
    let fake = Fake::start(vec![(
        "PUT",
        "/chat/v1/users/ME/conversations/",
        200,
        "{}".into(),
    )])
    .await;
    set_chat_hidden_with_client(&fake.client(), CHAT, false)
        .await
        .expect("unhide ok");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].path.ends_with("name=unpinnedTime"));
    assert_eq!(reqs[0].json(), json!({"unpinnedTime": null}));
}

#[tokio::test]
async fn hide_chat_failure_is_surfaced() {
    let fake = Fake::start(vec![(
        "PUT",
        "/chat/v1/users/ME/conversations/",
        403,
        "no".into(),
    )])
    .await;
    assert!(
        set_chat_hidden_with_client(&fake.client(), CHAT, true)
            .await
            .is_err()
    );
}

// ---- §FIXPACK F11: the three GRAPHSWEEP verifier leftovers ----

const ME_OID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const PEER_A: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const PEER_B: &str = "cccccccc-1111-2222-3333-444444444444";

/// Group create runs on the chat service (`POST /v1/threads`, skypetoken),
/// the new thread id is read from the Location header, and Graph is only
/// asked for the signed-in user's own object id (a GET, no Graph write).
#[tokio::test]
async fn create_group_chat_posts_thread_to_chat_service_and_reads_location() {
    use super::chat::{create_group_chat_data, group_chat_create_body};
    let fake = Fake::start(vec![
        ("GET", "/graph/me", 200, json!({"id": ME_OID}).to_string()),
        (
            "POST",
            "/chat/v1/threads",
            201,
            "@location:https://chat.example.com/v1/threads/19%3Agroup1%40thread.v2\n{}".into(),
        ),
    ])
    .await;
    let users = vec![PEER_A.to_string(), PEER_B.to_uppercase()];
    let chat = create_group_chat_data(&fake.client(), &users, Some("  Launch crew "))
        .await
        .expect("create ok");
    assert_eq!(chat.id, "19:group1@thread.v2");
    assert!(chat.is_group);
    assert_eq!(chat.name, "Launch crew");
    let reqs = fake.requests();
    let post = reqs
        .iter()
        .find(|r| r.method == "POST")
        .expect("one thread POST");
    assert_eq!(post.path, "/chat/v1/threads");
    assert_eq!(
        post.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!post.haystack().contains(GRAPH) && !post.haystack().contains(AAD));
    assert_eq!(
        post.json(),
        group_chat_create_body(
            ME_OID,
            &[PEER_A.to_string(), PEER_B.to_string()],
            Some("  Launch crew ")
        )
    );
    // Graph saw only the own-id GET.
    for g in reqs.iter().filter(|r| r.path.starts_with("/graph")) {
        assert_eq!(
            (g.method.as_str(), g.path.as_str()),
            ("GET", "/graph/me?$select=id")
        );
    }
}

#[tokio::test]
async fn create_group_chat_without_a_thread_id_is_an_error() {
    use super::chat::create_group_chat_data;
    let fake = Fake::start(vec![
        ("GET", "/graph/me", 200, json!({"id": ME_OID}).to_string()),
        ("POST", "/chat/v1/threads", 201, "{}".into()),
    ])
    .await;
    let err = create_group_chat_data(&fake.client(), &[PEER_A.to_string()], None)
        .await
        .err()
        .expect("no id = error");
    assert!(format!("{err:#}").contains("no thread id"), "{err:#}");
}

/// Unpin bails before any network (Graph pin delete is not granted).
#[tokio::test]
async fn unpin_bails_without_any_request() {
    use super::chat::chat_unpin_message_with_client;
    let fake = Fake::start(vec![]).await;
    let err = chat_unpin_message_with_client(&fake.client(), "19:a@thread.v2", "pin-1")
        .await
        .err()
        .expect("unpin bails");
    assert!(format!("{err:#}").contains("not available"), "{err:#}");
    let bad = chat_unpin_message_with_client(&fake.client(), "19:a@thread.v2", "a/b")
        .await
        .err()
        .expect("bad id");
    assert!(format!("{bad:#}").contains("bad chat or pin id"), "{bad:#}");
    assert!(fake.requests().is_empty(), "no request of any kind");
}

/// Reading pins is the chat service only: empty, populated and failing
/// answers never produce a Graph request.
#[tokio::test]
async fn pins_never_call_graph() {
    use super::chat::chat_pinned_messages_data;
    for (status, body) in [
        (200u16, "{}".to_string()),
        (500, "{}".to_string()),
        (403, "{}".to_string()),
    ] {
        let fake = Fake::start(vec![("GET", "/chat/v1/threads/", status, body)]).await;
        let out = chat_pinned_messages_data(&fake.client(), "19:a@thread.v2").await;
        assert_eq!(out.is_ok(), status == 200, "status {status}");
        let reqs = fake.requests();
        assert!(!reqs.is_empty());
        for r in &reqs {
            assert!(
                r.path.starts_with("/chat/"),
                "pins read only the chat service, saw {}",
                r.path
            );
            assert!(!r.haystack().contains(GRAPH));
        }
    }
}

// ---- MSGDELETE: own-message delete is the web client's softDelete ----

/// DELETE carries `behavior=softDelete` and the percent-encoded conversation id.
#[tokio::test]
async fn delete_message_sends_soft_delete_with_encoded_conversation() {
    let fake = Fake::start(vec![(
        "DELETE",
        "/chat/v1/users/ME/conversations/",
        200,
        "{}".into(),
    )])
    .await;
    delete_message_with_client(&fake.client(), "19:abc@thread.v2", "1700000000000")
        .await
        .expect("delete ok");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "DELETE");
    assert_eq!(
        reqs[0].path,
        "/chat/v1/users/ME/conversations/19%3Aabc%40thread.v2/messages/1700000000000?behavior=softDelete"
    );
    assert_eq!(
        reqs[0].headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!reqs[0].headers.contains_key("authorization"));
}

/// A refused delete is an error, never swallowed.
#[tokio::test]
async fn delete_message_failure_is_surfaced() {
    let fake = Fake::start(vec![(
        "DELETE",
        "/chat/v1/users/ME/conversations/",
        403,
        "no".into(),
    )])
    .await;
    assert!(
        delete_message_with_client(&fake.client(), "19:abc@thread.v2", "1")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rich_quote_reply_posts_markup_and_client_id_to_chat_service() {
    let fake = Fake::start(vec![(
        "POST",
        "/chat/v1/users/ME/conversations/chat-1/messages",
        201,
        json!({"OriginalArrivalTime": "1700000000000"}).to_string(),
    )])
    .await;
    let sent = reply_rich_message_with_client_id(
        &fake.client(),
        "chat-1",
        "1700000000000",
        "Ana <Build>",
        "Ship & verify",
        "<p><strong>Feito</strong></p>",
        "client-reply-1",
    )
    .await
    .expect("reply accepted by local transport");
    assert_eq!(sent.id.as_deref(), Some("1700000000000"));
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        "/chat/v1/users/ME/conversations/chat-1/messages"
    );
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(
        request.json(),
        json!({
            "content": "<quote author=\"Ana &lt;Build&gt;\" guid=\"1700000000000\">Ship &amp; verify</quote><p><strong>Feito</strong></p>",
            "messagetype": "RichText/Html",
            "contenttype": "text",
            "clientmessageid": "client-reply-1"
        })
    );
}

#[tokio::test]
async fn rich_channel_reply_posts_to_the_root_chain_without_chat_quote_markup() {
    let route = "/chat/v1/users/ME/conversations/19:chan@thread.tacv2;messageid=root-7/messages";
    let fake = Fake::start(vec![(
        "POST",
        route,
        201,
        json!({"OriginalArrivalTime": "1770000000008"}).to_string(),
    )])
    .await;
    let sent = thread_reply_rich_with_client_id(
        &fake.client(),
        CHANNEL,
        "root-7",
        "<p><strong>Resposta</strong></p>",
        "client-thread-reply-8",
    )
    .await
    .expect("thread reply accepted by local transport");
    assert_eq!(sent.id.as_deref(), Some("1770000000008"));

    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, route);
    assert_eq!(
        request.headers.get("authentication").map(String::as_str),
        Some(format!("skypetoken={SKYPE}").as_str())
    );
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(
        request.json(),
        json!({
            "content": "<p><strong>Resposta</strong></p>",
            "messagetype": "RichText/Html",
            "contenttype": "text",
            "clientmessageid": "client-thread-reply-8"
        })
    );
}

#[tokio::test]
async fn channel_reply_rejects_invalid_roots_before_network() {
    let fake = Fake::start(vec![]).await;
    assert!(thread_reply_rich_with_client_id(
        &fake.client(),
        CHANNEL,
        "root;injected",
        "<p>Resposta</p>",
        "client-thread-reply-9",
    )
    .await
    .is_err());
    assert!(fake.requests().is_empty());
}
